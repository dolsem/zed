import { effect } from "@preact/signals";
import { render } from "preact";
import { App } from "./app";
import { fetchSession, setToken, token } from "./connection";
import { conn, handleLocation, refreshThreads } from "./state";
import "./styles.css";
import { initViewport } from "./ui";

initViewport();
handleLocation();

// Connect whenever a token is present; validate it in the background.
effect(() => {
  if (token.value) {
    conn.start();
    void fetchSession().then((session) => {
      if (session === "unauthorized") {
        conn.stop();
        conn.revoked.value = true;
        setToken(null);
      }
    });
  } else {
    conn.stop();
  }
});

window.addEventListener("hashchange", () => handleLocation());

// iOS suspends the page (and silently kills sockets) in the background.
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState !== "visible") return;
  conn.resume();
  if (conn.isReady) void refreshThreads();
});
window.addEventListener("online", () => conn.resume());
window.addEventListener("pageshow", (event) => {
  if (event.persisted) conn.resume();
});

if ("serviceWorker" in navigator) {
  navigator.serviceWorker.addEventListener("message", (event: MessageEvent) => {
    const data = event.data as { type?: string; url?: string } | null;
    if (data?.type === "navigate" && typeof data.url === "string") handleLocation(data.url);
  });
  if (import.meta.env.PROD) {
    window.addEventListener("load", () => {
      navigator.serviceWorker.register("/sw.js", { scope: "/" }).catch((error: unknown) => {
        console.warn("Service worker registration failed", error);
      });
    });
  }
}

const root = document.getElementById("app");
if (root) render(<App />, root);
