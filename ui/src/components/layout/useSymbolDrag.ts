import { useCallback, useEffect, useRef, useState, type PointerEvent } from 'react';

type Target = { symbol: string; after: boolean };
type Drag = { id: number; symbol: string; startX: number; startY: number; x: number; y: number; active: boolean };

// Use pointer capture rather than the OS/WebView HTML5 drag session. This
// keeps move/up events in the page, including when the mouse leaves a row.
export function useSymbolDrag(enabled: boolean, move: (source: string, target: string, after: boolean) => void) {
  const listRef = useRef<HTMLUListElement>(null);
  const session = useRef<Drag | null>(null);
  const [dragging, setDragging] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<Target | null>(null);
  const cancel = useCallback(() => {
    session.current = null;
    setDragging(null);
    setDropTarget(null);
  }, []);
  const locate = useCallback((x: number, y: number): Target | null => {
    const list = listRef.current;
    if (!list) return null;
    const bounds = list.getBoundingClientRect();
    if (x < bounds.left || x > bounds.right || y < bounds.top || y > bounds.bottom) return null;
    const rows = Array.from(list.querySelectorAll<HTMLElement>('[data-sort-symbol]'));
    for (const row of rows) {
      const rect = row.getBoundingClientRect();
      if (y < rect.bottom) return { symbol: row.dataset.sortSymbol!, after: y >= rect.top + rect.height / 2 };
    }
    const last = rows.at(-1);
    return last ? { symbol: last.dataset.sortSymbol!, after: true } : null;
  }, []);
  const updateTarget = useCallback((drag: Drag) => {
    const target = locate(drag.x, drag.y);
    const next = target?.symbol === drag.symbol ? null : target;
    setDropTarget((prev) => prev?.symbol === next?.symbol && prev?.after === next?.after ? prev : next);
  }, [locate]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && session.current) { e.preventDefault(); cancel(); }
    };
    window.addEventListener('keydown', onKey);
    window.addEventListener('blur', cancel);
    return () => { window.removeEventListener('keydown', onKey); window.removeEventListener('blur', cancel); };
  }, [cancel]);

  useEffect(() => {
    if (!dragging) return;
    let frame = 0;
    const scroll = () => {
      const drag = session.current;
      const list = listRef.current;
      if (!drag || !list) return;
      const rect = list.getBoundingClientRect();
      if (drag.x >= rect.left && drag.x <= rect.right && drag.y >= rect.top && drag.y <= rect.bottom) {
        const delta = drag.y < rect.top + 28 ? -6 : drag.y > rect.bottom - 28 ? 6 : 0;
        if (delta) { list.scrollTop += delta; updateTarget(drag); }
      }
      frame = requestAnimationFrame(scroll);
    };
    frame = requestAnimationFrame(scroll);
    return () => cancelAnimationFrame(frame);
  }, [dragging, updateTarget]);

  const start = (e: PointerEvent<HTMLButtonElement>, symbol: string) => {
    if (!enabled || e.button !== 0 || !e.isPrimary) return;
    e.preventDefault();
    e.currentTarget.focus({ preventScroll: true });
    e.currentTarget.setPointerCapture(e.pointerId);
    session.current = { id: e.pointerId, symbol, startX: e.clientX, startY: e.clientY, x: e.clientX, y: e.clientY, active: false };
  };
  const track = (e: PointerEvent<HTMLButtonElement>) => {
    const drag = session.current;
    if (!drag || drag.id !== e.pointerId) return;
    if (!enabled || !(e.buttons & 1)) { cancel(); return; }
    drag.x = e.clientX; drag.y = e.clientY;
    if (!drag.active && Math.hypot(drag.x - drag.startX, drag.y - drag.startY) < 4) return;
    e.preventDefault();
    if (!drag.active) { drag.active = true; setDragging(drag.symbol); }
    updateTarget(drag);
  };
  const finish = (e: PointerEvent<HTMLButtonElement>) => {
    const drag = session.current;
    if (!drag || drag.id !== e.pointerId) return;
    const target = locate(e.clientX, e.clientY);
    if (enabled && drag.active && target) move(drag.symbol, target.symbol, target.after);
    cancel();
    if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
  };
  return { listRef, dragging, dropTarget, start, track, finish, cancel };
}
