// Theme handling: dark default, light palette in app.css
// via `:root[data-theme="light"]`. Persisted in localStorage; applied to
// the document root so every token flips at once.
const KEY = "ll-theme";

export function getTheme(): "dark" | "light" {
  return localStorage.getItem(KEY) === "light" ? "light" : "dark";
}

export function setTheme(t: "dark" | "light"): void {
  localStorage.setItem(KEY, t);
  document.documentElement.dataset.theme = t;
}

export function applyTheme(): void {
  document.documentElement.dataset.theme = getTheme();
}
