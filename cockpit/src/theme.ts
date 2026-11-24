import { useSyncExternalStore } from "react";

// Light or dark. The choice lives in localStorage ("oms-theme"); index.html applies
// it before the first paint, so there is no dark flash on a light page.
export type Theme = "dark" | "light";

export const THEME_KEY = "oms-theme";

export function parseTheme(v: string | null | undefined): Theme {
  return v === "light" ? "light" : "dark";
}

export function otherTheme(t: Theme): Theme {
  return t === "light" ? "dark" : "light";
}

const listeners = new Set<() => void>();

function current(): Theme {
  return document.documentElement.classList.contains("light") ? "light" : "dark";
}

export function setTheme(t: Theme): void {
  document.documentElement.classList.toggle("light", t === "light");
  document.documentElement.style.colorScheme = t;
  try { localStorage.setItem(THEME_KEY, t); } catch { /* private mode: not remembered */ }
  listeners.forEach((l) => l());
}

export function useTheme(): Theme {
  return useSyncExternalStore(
    (l) => { listeners.add(l); return () => listeners.delete(l); },
    current,
    () => "dark",
  );
}
