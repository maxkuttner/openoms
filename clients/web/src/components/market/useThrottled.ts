import { useEffect, useRef, useState } from "react";

/** `value`, but re-rendered at most once per `ms` (the latest value always lands). */
export function useThrottled<T>(value: T, ms: number): T {
  const [shown, setShown] = useState(value);
  const last = useRef(0);
  const pending = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    const wait = last.current + ms - Date.now();
    if (wait <= 0) {
      last.current = Date.now();
      setShown(value);
      return;
    }
    if (pending.current) clearTimeout(pending.current);
    pending.current = setTimeout(() => { last.current = Date.now(); setShown(value); }, wait);
    return () => { if (pending.current) clearTimeout(pending.current); };
  }, [value, ms]);
  return shown;
}
