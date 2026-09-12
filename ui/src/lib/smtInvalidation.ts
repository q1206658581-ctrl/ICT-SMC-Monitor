import type { SmtInvalidationReason } from '../types/structures';

function ticker(symbol: string) {
  return symbol.split(':').pop() ?? symbol;
}

export function formatSmtInvalidationReasons(
  reasons: SmtInvalidationReason[] | undefined,
  sweeperSymbol: string,
  emptyLabel = '旧记录未保存原因',
) {
  const labels: Record<SmtInvalidationReason, string> = {
    sweeper_c2_failed: `${ticker(sweeperSymbol)} C2确认失败`,
    sweeper_c3_failed: `${ticker(sweeperSymbol)} C3结构失效`,
    all_counters_swept: '所有对手品种均清扫，无SMT背离',
    canonical_reference_changed: '历史复核：清扫参照已变化',
    canonical_sweep_invalid: '历史复核：无法复现HTF清扫',
    reference_already_taken: '历史复核：该流动性此前已被取走',
    canonical_chain_changed: '历史复核：C1/C2/C3链无法完整复现',
    pda_consumed_by_other_smt: '同一PDA已由更早确认C2的SMT占用',
    htf_formation_cancelled: 'HTF形成中背离已撤销',
  };
  return reasons && reasons.length > 0
    ? reasons.map((reason) => labels[reason] ?? reason).join('；')
    : emptyLabel;
}
