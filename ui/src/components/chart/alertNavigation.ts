/** Inbox audit navigation must still resolve snapshots hidden by public SMT filters. */
export async function resolveAlertSmt<T extends { id: string }>(
  smtId: string,
  visible: T[],
  loadSnapshot: (id: string) => Promise<T | null>,
): Promise<T | null> {
  return visible.find((item) => item.id === smtId) ?? await loadSnapshot(smtId);
}
