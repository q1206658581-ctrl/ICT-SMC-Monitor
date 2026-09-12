use crate::types::{Bar, Timeframe};

use crate::detector::engine::registry::detector_kind_tags;
use crate::detector::engine::state::IctEngine;
use crate::detector::swing::SwingSeries;
use crate::detector::types::{IctStructure, StructureEvent};

impl IctEngine {
    fn apply_shared_swing_fractal(&mut self, value: &serde_json::Value, replay: bool) -> usize {
        let Some(n) = value.as_u64().map(|value| (value as usize).max(1)) else {
            return 0;
        };
        let keys: Vec<(String, Timeframe)> = self.streams.keys().cloned().collect();
        let mut applied = 0usize;
        for key in keys {
            let Some(bucket) = self.streams.get_mut(&key) else {
                continue;
            };
            let mut has_mss = false;
            for detector in &mut bucket.detectors {
                if detector.name() == "mss" {
                    has_mss |= detector.apply_param("fractal_n", value);
                }
            }
            if !has_mss {
                continue;
            }
            applied += 1;
            let history: Vec<Bar> = bucket.history.iter().cloned().collect();
            let mut rebuilt = SwingSeries::new(n);
            for end in 1..=history.len() {
                rebuilt.on_closed_bar(&history[..end]);
            }
            bucket.swings = rebuilt;
        }
        if !replay || applied == 0 {
            return applied;
        }

        // fractal_n belongs to the one shared SwingSeries, so every detector
        // that reads swing highs/lows must be rebuilt on the same definition.
        // Replaying MSS alone would leave contradictory levels on the chart.
        for detector in ["mss", "liquidity", "bos", "ote", "premium_discount"] {
            self.invalidate_and_reset_detector(detector);
            self.replay_detector_with_history(detector);
        }
        let reversal_events = self.rebuild_liquidity_reversals();
        for event in reversal_events {
            self.apply_and_broadcast(event);
        }
        applied
    }

    pub fn set_detector_enabled(&mut self, name: &str, enabled: bool) {
        if is_display_only_toggle(name) {
            tracing::info!(
                detector = %name,
                enabled,
                "display-only detector toggle ignored by engine"
            );
            return;
        }
        let was_enabled = self.toggles.is_enabled(name);
        self.toggles.set(name, enabled);
        if was_enabled == enabled {
            return;
        }
        if name == "liquidity_reversal" {
            let events = if enabled {
                self.rebuild_liquidity_reversals()
            } else {
                Vec::new()
            };
            for ev in events {
                self.apply_and_broadcast(ev);
            }
            return;
        }
        if enabled {
            // Replay history so structures the detector would have
            // produced reappear immediately.
            let _ = self.replay_detector_with_history(name);
        } else {
            // Tear down: invalidate every structure this detector owns
            // and reset its internal state machine so re-enabling starts
            // from a clean slate.
            self.invalidate_and_reset_detector(name);
        }
    }

    /// Drops all structures currently owned by `name` (across every
    /// (symbol,tf) bucket) and emits `Invalidated` for each. Also calls
    /// `reset()` on each matching detector instance.

    pub fn apply_detector_param(
        &mut self,
        name: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> usize {
        if name == "mss" && key == "fractal_n" {
            return self.apply_shared_swing_fractal(value, true);
        }
        if name == "liquidity_reversal" {
            return self.apply_liquidity_reversal_param(key, value);
        }
        let kinds = detector_kind_tags(name);
        let mut applied = 0usize;
        let mut events: Vec<StructureEvent> = Vec::new();

        // Collect (symbol, tf) keys first so we can mutate streams freely.
        let keys: Vec<(String, Timeframe)> = self.streams.keys().cloned().collect();

        if matches!(name, "pdh_pdl") {
            tracing::info!(
                target: "pdh_pdl_trace",
                stage = "apply_detector_param:start",
                name, key, %value,
                bucket_count = keys.len(),
                "engine apply_detector_param entered"
            );
        }

        for key_pair in keys {
            // Process this bucket: apply param to matching detector(s),
            // collect ids of structures to invalidate, replay history.
            let bucket = match self.streams.get_mut(&key_pair) {
                Some(b) => b,
                None => continue,
            };

            let mut detector_was_reset = false;
            for det in bucket.detectors.iter_mut() {
                if det.name() != name {
                    continue;
                }
                if det.apply_param(key, value) {
                    applied += 1;
                    detector_was_reset = true;
                }
            }
            if !detector_was_reset {
                continue;
            }
            // Display-only enable/disable toggles are config-only: the
            // front-end OverlayFilter handles show/hide. Skip the
            // destructive reset + invalidate + replay to preserve existing
            // structures.
            if is_display_only_toggle(name) && (key == "enabled" || key.starts_with("enabled_")) {
                continue;
            }
            for det in bucket.detectors.iter_mut() {
                if det.name() == name {
                    det.reset();
                }
            }

            // Invalidate this detector's existing structures in the global
            // map (filter by kind tag + matching symbol/tf).
            let to_remove: Vec<String> = self
                .structures_index
                .get(&(key_pair.0.clone(), key_pair.1))
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| {
                            let s = self.structures.get(id)?;
                            if kinds.contains(&s.value().kind_tag()) {
                                Some(id.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            if matches!(name, "pdh_pdl") {
                tracing::info!(
                    target: "pdh_pdl_trace",
                    stage = "apply_detector_param:invalidate_old",
                    sym = %key_pair.0,
                    tf = ?key_pair.1,
                    ids = ?to_remove,
                    "removing existing pdh/pdl structures before replay"
                );
            }
            for id in &to_remove {
                if let Some(s) = self.structures.get(id) {
                    let kind = s.kind_tag().to_string();
                    drop(s);
                    events.push(StructureEvent::Invalidated {
                        id: id.clone(),
                        kind,
                    });
                }
            }

            // Replay history through this detector only. We feed growing
            // slices so detectors that key off `last bar` advance one step
            // at a time (FVG's 3-bar window, OB's lookback, etc.).
            let history_slice: Vec<Bar> = bucket.history.iter().cloned().collect();
            // Re-run swing detection step-by-step (instead of reusing a
            // fully-populated snapshot) so swing-dependent detectors
            // (MSS/CISD/BoS) see the same incremental SwingDelta sequence
            // they would in a live feed. Also collect structures_snapshot
            // per step so the detector sees cross-TF context.
            let pre_event_len = events.len();
            let history_len = history_slice.len();
            let mut replay_swings = SwingSeries::new(bucket.swings.fractal_n);
            for det in bucket.detectors.iter_mut() {
                if det.name() != name {
                    continue;
                }
                replay_swings = SwingSeries::new(bucket.swings.fractal_n);
                for end in 1..=history_slice.len() {
                    let slice = &history_slice[..end];
                    let _ = replay_swings.on_closed_bar(slice);
                    // Use the index-backed snapshot (O(matching) instead
                    // of O(total_structures) full scan per bar step).
                    // Access fields directly to allow split-borrowing
                    // alongside the mutable `bucket` from self.streams.
                    let structures_snapshot: Vec<IctStructure> = if name == "po3" {
                        let mut result = Vec::new();
                        for entry in self.structures_index.iter() {
                            if entry.key().0 != key_pair.0 {
                                continue;
                            }
                            for id in entry.value() {
                                if let Some(s) = self.structures.get(id) {
                                    result.push(s.value().clone());
                                }
                            }
                        }
                        result
                    } else {
                        self.structures_index
                            .get(&(key_pair.0.clone(), key_pair.1))
                            .map(|ids| {
                                ids.iter()
                                    .filter_map(|id| {
                                        self.structures.get(id).map(|s| s.value().clone())
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    let ctx = crate::detector::DetectorCtx {
                        swings: &replay_swings,
                        structures: &structures_snapshot,
                    };
                    let mut evs = det.on_closed(slice, &ctx);
                    events.append(&mut evs);
                }
                let mut flushed = det.flush(&history_slice);
                events.append(&mut flushed);
            }
            drop(replay_swings);
            if matches!(name, "pdh_pdl") {
                let new_evs: Vec<String> = events[pre_event_len..]
                    .iter()
                    .map(|e| match e {
                        StructureEvent::New(s) => format!("New({},{})", s.kind_tag(), s.id()),
                        StructureEvent::Update(s) => format!("Update({},{})", s.kind_tag(), s.id()),
                        StructureEvent::Invalidated { id, kind } => format!("Inv({},{})", kind, id),
                    })
                    .collect();
                tracing::info!(
                    target: "pdh_pdl_trace",
                    stage = "apply_detector_param:replay_done",
                    sym = %key_pair.0,
                    tf = ?key_pair.1,
                    history_len,
                    emitted = ?new_evs,
                    "replay produced events"
                );
            }
        }

        if matches!(name, "pdh_pdl") {
            tracing::info!(
                target: "pdh_pdl_trace",
                stage = "apply_detector_param:flush",
                total_events = events.len(),
                applied,
                "broadcasting all collected events"
            );
        }
        for ev in events {
            self.apply_and_broadcast(ev);
        }
        applied
    }

    /// Cold-start variant of [`apply_detector_params`]: writes the config
    /// onto every matching detector instance but **does not** tear down or
    /// replay history. On startup the engine adopts the persisted detector
    /// settings without invalidating the structures hydrated from SQLite —
    /// a full replay here would run over the still-partial bucket history
    /// (cold-start seed only replays the recent N bars, TV backfill lands
    /// later) and wipe the hydrated PO3 set, leaving the chart showing a
    /// partial count until the user manually toggles the detector. The
    /// post-backfill `replay_po3_full_history` + `po3_replay_done` re-fetch
    /// delivers the complete set; live bars adopt the new config going
    /// forward. User-initiated edits still go through `apply_detector_param`
    /// (which replays) so changing a threshold re-detects immediately.
    pub fn apply_detector_params_config_only(
        &mut self,
        name: &str,
        updates: &[(String, serde_json::Value)],
    ) -> usize {
        if updates.is_empty() {
            return 0;
        }
        if name == "mss" {
            if let Some((_, value)) = updates.iter().find(|(key, _)| key == "fractal_n") {
                return self.apply_shared_swing_fractal(value, false);
            }
        }
        let keys: Vec<(String, Timeframe)> = self.streams.keys().cloned().collect();
        let mut applied = 0usize;
        for key_pair in keys {
            let bucket = match self.streams.get_mut(&key_pair) {
                Some(b) => b,
                None => continue,
            };
            for det in bucket.detectors.iter_mut() {
                if det.name() != name {
                    continue;
                }
                for (key, value) in updates {
                    if det.apply_param(key, value) {
                        applied += 1;
                    }
                }
            }
        }
        applied
    }

    pub fn apply_detector_params(
        &mut self,
        name: &str,
        updates: &[(String, serde_json::Value)],
    ) -> usize {
        if updates.is_empty() {
            return 0;
        }
        if updates.len() == 1 {
            let (key, value) = &updates[0];
            return self.apply_detector_param(name, key, value);
        }
        if name == "liquidity_reversal" {
            let mut applied = 0usize;
            for (key, value) in updates {
                applied += self.apply_liquidity_reversal_param(key, value);
            }
            return applied;
        }

        let kinds = detector_kind_tags(name);
        let keys: Vec<(String, Timeframe)> = self.streams.keys().cloned().collect();
        let mut applied = 0usize;
        let mut events: Vec<StructureEvent> = Vec::new();

        for key_pair in keys {
            let bucket = match self.streams.get_mut(&key_pair) {
                Some(b) => b,
                None => continue,
            };

            let mut detector_was_reset = false;
            for det in bucket.detectors.iter_mut() {
                if det.name() != name {
                    continue;
                }
                let mut changed = false;
                for (key, value) in updates {
                    if det.apply_param(key, value) {
                        applied += 1;
                        changed = true;
                    }
                }
                detector_was_reset |= changed;
            }
            if !detector_was_reset {
                continue;
            }
            // Display-only enable/disable toggles are config-only.
            let all_enabled_toggles = updates
                .iter()
                .all(|(k, _)| k == "enabled" || k.starts_with("enabled_"));
            if is_display_only_toggle(name) && all_enabled_toggles {
                continue;
            }
            for det in bucket.detectors.iter_mut() {
                if det.name() == name {
                    det.reset();
                }
            }

            let to_remove: Vec<String> = self
                .structures_index
                .get(&(key_pair.0.clone(), key_pair.1))
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| {
                            let s = self.structures.get(id)?;
                            if kinds.contains(&s.value().kind_tag()) {
                                Some(id.clone())
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            for id in &to_remove {
                if let Some(s) = self.structures.get(id) {
                    let kind = s.kind_tag().to_string();
                    drop(s);
                    events.push(StructureEvent::Invalidated {
                        id: id.clone(),
                        kind,
                    });
                }
            }

            let history_slice: Vec<Bar> = bucket.history.iter().cloned().collect();
            let mut replay_swings = SwingSeries::new(bucket.swings.fractal_n);
            for det in bucket.detectors.iter_mut() {
                if det.name() != name {
                    continue;
                }
                replay_swings = SwingSeries::new(bucket.swings.fractal_n);
                for end in 1..=history_slice.len() {
                    let slice = &history_slice[..end];
                    let _ = replay_swings.on_closed_bar(slice);
                    // Use the index-backed snapshot (O(matching) instead
                    // of O(total_structures) full scan per bar step).
                    // Access fields directly to allow split-borrowing
                    // alongside the mutable `bucket` from self.streams.
                    let structures_snapshot: Vec<IctStructure> = if name == "po3" {
                        let mut result = Vec::new();
                        for entry in self.structures_index.iter() {
                            if entry.key().0 != key_pair.0 {
                                continue;
                            }
                            for id in entry.value() {
                                if let Some(s) = self.structures.get(id) {
                                    result.push(s.value().clone());
                                }
                            }
                        }
                        result
                    } else {
                        self.structures_index
                            .get(&(key_pair.0.clone(), key_pair.1))
                            .map(|ids| {
                                ids.iter()
                                    .filter_map(|id| {
                                        self.structures.get(id).map(|s| s.value().clone())
                                    })
                                    .collect()
                            })
                            .unwrap_or_default()
                    };
                    let ctx = crate::detector::DetectorCtx {
                        swings: &replay_swings,
                        structures: &structures_snapshot,
                    };
                    let mut evs = det.on_closed(slice, &ctx);
                    events.append(&mut evs);
                }
                let mut flushed = det.flush(&history_slice);
                events.append(&mut flushed);
            }
            drop(replay_swings);
        }

        for ev in events {
            self.apply_and_broadcast(ev);
        }
        applied
    }

    fn apply_liquidity_reversal_param(&mut self, key: &str, value: &serde_json::Value) -> usize {
        let applied = match key {
            "max_bars_after_sweep" => value
                .as_u64()
                .map(|v| self.reversal_cfg.max_bars_after_sweep = v.max(1) as usize)
                .is_some(),
            "allow_cisd" => value
                .as_bool()
                .map(|v| self.reversal_cfg.allow_cisd = v)
                .is_some(),
            "allow_mss" => value
                .as_bool()
                .map(|v| self.reversal_cfg.allow_mss = v)
                .is_some(),
            "min_score" => value
                .as_u64()
                .map(|v| self.reversal_cfg.min_score = v.min(255) as u8)
                .is_some(),
            _ => false,
        };
        if !applied {
            return 0;
        }
        let events = self.rebuild_liquidity_reversals();
        for ev in events {
            self.apply_and_broadcast(ev);
        }
        1
    }
}

fn is_display_only_toggle(name: &str) -> bool {
    matches!(
        name,
        "fvg"
            | "order_block"
            | "mss"
            | "cisd"
            | "pdh_pdl"
            | "liquidity"
            | "liquidity_reversal"
            | "bos"
            | "breaker_block"
            | "volume_imbalance"
            | "ote"
            | "premium_discount"
            | "opening_gap"
            | "session"
            | "po3"
    )
}
