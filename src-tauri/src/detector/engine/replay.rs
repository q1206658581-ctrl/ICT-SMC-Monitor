use crate::types::{Bar, Timeframe};

use crate::detector::engine::registry::detector_kind_tags;
use crate::detector::engine::state::IctEngine;
use crate::detector::swing::SwingSeries;
use crate::detector::types::{IctStructure, StructureEvent};

impl IctEngine {
    pub fn list_active(&self, symbol: &str, tf: Timeframe) -> Vec<IctStructure> {
        const CROSS_TF: &[&str] = &[
            "pdh",
            "pdl",
            "kill_zone",
            "nwog",
            "ndog",
            "session_range",
            "kill_zone_window",
        ];
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for entry in self.structures_index.iter() {
            if entry.key().0 != symbol {
                continue;
            }
            for id in entry.value() {
                if !seen.insert(id.clone()) {
                    continue;
                }
                if let Some(s) = self.structures.get(id) {
                    let st = s.value();
                    if st.tf() == tf || CROSS_TF.contains(&st.kind_tag()) {
                        result.push(st.clone());
                    }
                }
            }
        }
        result.sort_by(|a, b| a.id().cmp(b.id()));
        result
    }

    /// Returns a stable snapshot of every structure currently owned by the
    /// engine.  Offline replay uses this rather than reaching into the
    /// concurrent maps so output is deterministic across runs.
    pub fn list_all_structures(&self) -> Vec<IctStructure> {
        let mut result: Vec<IctStructure> = self
            .structures
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        result.sort_by(|a, b| a.id().cmp(b.id()));
        result
    }

    /// Returns the count of currently-tracked structures owned by
    /// detector `name`. Used by toggle handlers to log delta around an
    /// enable/disable so we can verify replay actually fired.
    pub fn structures_count_for(&self, name: &str) -> usize {
        let kinds = detector_kind_tags(name);
        if kinds.is_empty() {
            return 0;
        }
        self.structures
            .iter()
            .filter(|kv| kinds.contains(&kv.value().kind_tag()))
            .count()
    }

    /// Inject a structure (used for cold-start hydration from SQLite).
    pub fn hydrate(&self, s: IctStructure) {
        let id = s.id().to_string();
        let sym = s.symbol().to_string();
        let tf = s.tf();
        let new_key = (sym, tf);
        let old_key = self
            .structures
            .get(&id)
            .map(|old| (old.symbol().to_string(), old.tf()));
        self.structures.insert(id.clone(), s);

        // `structures_index` is a Vec because its insertion order is used by
        // deterministic detector replay. Do not linearly scan that Vec for
        // every hydrated row: a 300k-row database made startup effectively
        // quadratic. The primary map already tells us whether the id existed.
        match old_key {
            None => self.structures_index.entry(new_key).or_default().push(id),
            Some(old_key) if old_key != new_key => {
                if let Some(mut ids) = self.structures_index.get_mut(&old_key) {
                    ids.retain(|existing| existing != &id);
                }
                self.structures_index.entry(new_key).or_default().push(id);
            }
            Some(_) => {}
        }
    }

    pub(super) fn invalidate_and_reset_detector(&mut self, name: &str) {
        let kinds = detector_kind_tags(name);
        if kinds.is_empty() {
            return;
        }
        let keys: Vec<(String, Timeframe)> = self.streams.keys().cloned().collect();
        let mut events: Vec<StructureEvent> = Vec::new();
        for key in keys {
            let bucket = match self.streams.get_mut(&key) {
                Some(b) => b,
                None => continue,
            };
            let mut hit = false;
            for det in bucket.detectors.iter_mut() {
                if det.name() == name {
                    det.reset();
                    hit = true;
                }
            }
            if !hit {
                continue;
            }
            let to_remove: Vec<(String, String)> = self
                .structures_index
                .get(&(key.0.clone(), key.1))
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| {
                            let s = self.structures.get(id)?;
                            let st = s.value();
                            if kinds.contains(&st.kind_tag()) {
                                Some((id.clone(), st.kind_tag().to_string()))
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            for (id, kind) in &to_remove {
                events.push(StructureEvent::Invalidated {
                    id: id.clone(),
                    kind: kind.clone(),
                });
            }
        }
        for ev in events {
            self.apply_and_broadcast(ev);
        }
    }

    /// Replays each (symbol,tf) bucket's history through `name`'s detector
    /// instance so it re-emits structures. Caller has already cleared old
    /// state via `invalidate_and_reset_detector` (or the detector was
    /// freshly toggled on with no prior structures). Returns instance count.
    pub(super) fn replay_detector_with_history(&mut self, name: &str) -> usize {
        let mut applied = 0usize;
        let mut events: Vec<StructureEvent> = Vec::new();
        let keys: Vec<(String, Timeframe)> = self.streams.keys().cloned().collect();
        for key in keys {
            let bucket = match self.streams.get_mut(&key) {
                Some(b) => b,
                None => continue,
            };
            // Reset before replay so per-detector state machines start clean.
            let mut matched = false;
            for det in bucket.detectors.iter_mut() {
                if det.name() == name {
                    det.reset();
                    matched = true;
                }
            }
            if !matched {
                continue;
            }
            let history_slice: Vec<Bar> = bucket.history.iter().cloned().collect();
            // Replay events are buffered until every bucket has finished, so
            // this view cannot change during the per-bar loop. Rebuilding and
            // cloning every structure for every historical bar made startup
            // effectively O(history * structures) and starved live quotes.
            let structures_snapshot: Vec<IctStructure> = if name == "po3" {
                let mut result = Vec::new();
                for entry in self.structures_index.iter() {
                    if entry.key().0 != key.0 {
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
                    .get(&(key.0.clone(), key.1))
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| self.structures.get(id).map(|s| s.value().clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            // Replay the swing series from scratch so MSS/CISD see the
            // same SwingDelta sequence they would in a live run. Without
            // this the engine reuses a fully-populated SwingSeries
            // snapshot, but each detector iteration replays bars[..end]
            // from the empty state — and our MssDetector::on_closed only
            // reads `last_of(SwingKind::*)`, which never updates across
            // the replay loop because we hand it the same swings_snapshot
            // on every step. Net effect: only the very last bar's MSS
            // condition is ever evaluated, so toggle-on emits 3-4 MSS
            // instead of the ~169 the live run accumulated.
            let mut replay_swings = SwingSeries::new(bucket.swings.fractal_n);
            for det in bucket.detectors.iter_mut() {
                if det.name() != name {
                    continue;
                }
                applied += 1;
                replay_swings = SwingSeries::new(bucket.swings.fractal_n);
                for end in 1..=history_slice.len() {
                    let slice = &history_slice[..end];
                    let _ = replay_swings.on_closed_bar(slice);
                    let ctx = crate::detector::DetectorCtx {
                        swings: &replay_swings,
                        structures: &structures_snapshot,
                    };
                    let mut evs = det.on_closed(slice, &ctx);
                    events.append(&mut evs);
                }
                // Surface running-state structures (e.g. PDH/PDL) that
                // the on_closed pass alone would only emit on rollover.
                let mut flushed = det.flush(&history_slice);
                events.append(&mut flushed);
            }
            // Discard replay_swings; the live `bucket.swings` is the
            // authoritative SwingSeries for subsequent on_closed_bar
            // calls — replay_swings was per-detector scratch state.
            drop(replay_swings);
        }
        for ev in events {
            self.apply_and_broadcast(ev);
        }
        applied
    }

    /// Cold-start replay of the PO3 detector for a single symbol's buckets.
    /// Called after that symbol's TV Historical backfill lands so every
    /// historical PO3 is detected — the cold-start seed only had ~500 bars
    /// and skipped po3 (truncated window spuriously invalidates PO3 whose
    /// lifecycle extends past the edge). This is the same invalidate +
    /// replay path the manual detector toggle uses, but scoped to one
    /// symbol so the engine lock is held for a shorter time and switching
    /// symbols during the replay is not blocked.

    /// Count active power_of_3 structures for a symbol. Used to decide
    /// whether to skip the PO3 replay (hydrated structures already exist).
    pub fn po3_count_for_symbol(&self, symbol: &str) -> usize {
        let mut count = 0usize;
        for entry in self.structures_index.iter() {
            if entry.key().0 != symbol {
                continue;
            }
            for id in entry.value() {
                if let Some(s) = self.structures.get(id) {
                    if s.value().kind_tag() == "power_of_3" {
                        count += 1;
                    }
                }
            }
        }
        count
    }
    pub fn replay_po3_for_symbol(&mut self, symbol: &str) -> usize {
        self.invalidate_and_reset_detector_for_symbol("po3", symbol);
        let n = self.replay_detector_with_history_for_symbol("po3", symbol);
        self.po3_replay_done.insert(symbol.to_string());
        let po3_count = {
            let mut count = 0usize;
            for entry in self.structures_index.iter() {
                if entry.key().0 != symbol {
                    continue;
                }
                for id in entry.value() {
                    if let Some(s) = self.structures.get(id) {
                        if s.value().kind_tag() == "power_of_3" {
                            count += 1;
                        }
                    }
                }
            }
            count
        };
        tracing::info!(%symbol, replayed = n, po3_in_structures = po3_count, "replay_po3_for_symbol: structures after replay");
        n
    }

    /// Symbol-scoped variant of [`invalidate_and_reset_detector`]: only
    /// touches buckets and structures belonging to `symbol`.
    fn invalidate_and_reset_detector_for_symbol(&mut self, name: &str, symbol: &str) {
        let kinds = detector_kind_tags(name);
        if kinds.is_empty() {
            return;
        }
        let keys: Vec<(String, Timeframe)> = self
            .streams
            .keys()
            .filter(|(s, _)| s == symbol)
            .cloned()
            .collect();
        let mut events: Vec<StructureEvent> = Vec::new();
        for key in keys {
            let bucket = match self.streams.get_mut(&key) {
                Some(b) => b,
                None => continue,
            };
            let mut hit = false;
            for det in bucket.detectors.iter_mut() {
                if det.name() == name {
                    det.reset();
                    hit = true;
                }
            }
            if !hit {
                continue;
            }
            let to_remove: Vec<String> = self
                .structures_index
                .get(&(key.0.clone(), key.1))
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
                    let kind = s.value().kind_tag().to_string();
                    drop(s);
                    events.push(StructureEvent::Invalidated {
                        id: id.clone(),
                        kind,
                    });
                }
            }
        }
        for ev in events {
            self.apply_and_broadcast(ev);
        }
    }

    /// Symbol-scoped variant of [`replay_detector_with_history`]: only
    /// replays buckets belonging to `symbol`.
    fn replay_detector_with_history_for_symbol(&mut self, name: &str, symbol: &str) -> usize {
        let mut applied = 0usize;
        let mut events: Vec<StructureEvent> = Vec::new();
        let keys: Vec<(String, Timeframe)> = self
            .streams
            .keys()
            .filter(|(s, _)| s == symbol)
            .cloned()
            .collect();
        for key in keys {
            let bucket = match self.streams.get_mut(&key) {
                Some(b) => b,
                None => continue,
            };
            let mut matched = false;
            for det in bucket.detectors.iter_mut() {
                if det.name() == name {
                    det.reset();
                    matched = true;
                }
            }
            if !matched {
                continue;
            }
            let history_slice: Vec<Bar> = bucket.history.iter().cloned().collect();
            // The structure store is unchanged until the buffered replay
            // events are applied below. One snapshot per bucket is therefore
            // both correct and dramatically cheaper than one clone per bar.
            let structures_snapshot: Vec<IctStructure> = if name == "po3" {
                let mut result = Vec::new();
                for entry in self.structures_index.iter() {
                    if entry.key().0 != key.0 {
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
                    .get(&(key.0.clone(), key.1))
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| self.structures.get(id).map(|s| s.value().clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let mut replay_swings = SwingSeries::new(bucket.swings.fractal_n);
            for det in bucket.detectors.iter_mut() {
                if det.name() != name {
                    continue;
                }
                applied += 1;
                replay_swings = SwingSeries::new(bucket.swings.fractal_n);
                for end in 1..=history_slice.len() {
                    let slice = &history_slice[..end];
                    let _ = replay_swings.on_closed_bar(slice);
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
        let event_count = events.len();
        for ev in events {
            self.apply_and_broadcast(ev);
        }
        tracing::info!(%name, %symbol, buckets = applied, events = event_count, "replay_detector_with_history_for_symbol: events generated");
        applied
    }
}
