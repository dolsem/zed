import type { ComponentType } from "preact";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { deviceName, HttpError, pairDevice, setDeviceName, setToken, token } from "./connection";
import {
  conn,
  dismissToast,
  errorMessage,
  overlay,
  panel,
  PANEL_TABS,
  PANEL_THREAD,
  PANEL_THREADS,
  pendingPairCode,
  setPanelNavigator,
  toasts,
} from "./state";
import { TabsView } from "./tabs";
import { ThreadView } from "./thread";
import { ThreadsView } from "./threads";
import { defaultDeviceName, haptic, Icon, isIOS, isStandalone, Spinner } from "./ui";

export function App() {
  const hasToken = token.value !== null;
  return (
    <>
      {hasToken ? <Main /> : <PairingScreen />}
      <Toasts />
      <div id="layers" />
    </>
  );
}

function Main() {
  return (
    <div class="main">
      <ConnectionBanner />
      <Panels />
      <LazyItemOverlay />
    </div>
  );
}

// The item views pull in CodeMirror; load them right after the first paint
// instead of blocking launch on them.
function LazyItemOverlay() {
  const [Overlay, setOverlay] = useState<ComponentType | null>(null);
  useEffect(() => {
    import("./itemview").then(
      (module) => setOverlay(() => module.ItemOverlay),
      (error: unknown) => console.error("Failed to load the item views", error),
    );
  }, []);
  return Overlay ? <Overlay /> : null;
}

function Panels() {
  const ref = useRef<HTMLDivElement>(null);
  const locked = overlay.value !== null;

  useLayoutEffect(() => {
    const element = ref.current;
    if (!element) return;
    setPanelNavigator((index, smooth = true) => {
      const width = element.clientWidth;
      panel.value = index;
      element.scrollTo({ left: index * width, behavior: smooth ? "smooth" : "auto" });
    });
    element.scrollLeft = panel.peek() * element.clientWidth;

    let frame = 0;
    const onScroll = () => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        const width = element.clientWidth || 1;
        const index = Math.round(element.scrollLeft / width);
        // Only commit once the snap settles near a panel boundary.
        if (Math.abs(element.scrollLeft - index * width) < width * 0.08 && index !== panel.peek()) {
          panel.value = index;
          (document.activeElement as HTMLElement | null)?.blur?.();
        }
      });
    };
    const keepAligned = () => {
      element.scrollLeft = panel.peek() * element.clientWidth;
    };
    element.addEventListener("scroll", onScroll, { passive: true });
    const observer = new ResizeObserver(keepAligned);
    observer.observe(element);
    return () => {
      element.removeEventListener("scroll", onScroll);
      observer.disconnect();
      cancelAnimationFrame(frame);
    };
  }, []);

  return (
    <div class={`panels ${locked ? "locked" : ""}`} ref={ref} aria-hidden={locked}>
      <section class="panel" aria-label="Threads" data-active={panel.value === PANEL_THREADS}>
        <ThreadsView />
      </section>
      <section class="panel" aria-label="Thread" data-active={panel.value === PANEL_THREAD}>
        <ThreadView />
      </section>
      <section class="panel" aria-label="Tabs" data-active={panel.value === PANEL_TABS}>
        <TabsView />
      </section>
    </div>
  );
}

function ConnectionBanner() {
  const status = conn.status.value;
  const everConnected = conn.generationSignal.value > 0;
  const [visible, setVisible] = useState(false);
  const problem = status !== "ready";

  // Brief blips (e.g. returning from background) shouldn't flash a banner.
  useEffect(() => {
    if (!problem) {
      setVisible(false);
      return;
    }
    const timer = setTimeout(() => setVisible(true), everConnected ? 700 : 1200);
    return () => clearTimeout(timer);
  }, [problem, everConnected]);

  return (
    <div class={`connection-banner ${visible && problem ? "shown" : ""}`} role="status" aria-live="polite">
      <Spinner size={13} />
      <span>{everConnected ? "Reconnecting…" : "Connecting to Zed…"}</span>
      {status === "offline" && (
        <button class="banner-retry" onClick={() => conn.resume()}>
          Retry
        </button>
      )}
    </div>
  );
}

function Toasts() {
  return (
    <div class="toasts" aria-live="polite">
      {toasts.value.map((toast) => (
        <button key={toast.id} class={`toast ${toast.level}`} onClick={() => dismissToast(toast.id)}>
          <Icon name={toast.level === "error" ? "alert" : "info"} size={16} />
          <span>{toast.message}</span>
        </button>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Pairing

function formatCode(raw: string): string {
  const characters = raw.toUpperCase().replace(/[^A-Z0-9]/g, "").slice(0, 8);
  return characters.length > 4 ? `${characters.slice(0, 4)}-${characters.slice(4)}` : characters;
}

function PairingScreen() {
  const [code, setCode] = useState(() => formatCode(pendingPairCode.peek() ?? ""));
  const [name, setName] = useState(() => deviceName.peek() ?? defaultDeviceName());
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const revoked = conn.revoked.value;
  const prefill = pendingPairCode.value;

  useEffect(() => {
    if (prefill) setCode(formatCode(prefill));
  }, [prefill]);

  const complete = code.replace("-", "").length === 8;

  const submit = async (event: Event) => {
    event.preventDefault();
    if (!complete || busy) return;
    haptic();
    setBusy(true);
    setError(null);
    try {
      const deviceToken = await pairDevice(code, name.trim() || defaultDeviceName());
      setDeviceName(name.trim() || defaultDeviceName());
      pendingPairCode.value = null;
      setToken(deviceToken);
    } catch (reason) {
      setError(
        reason instanceof HttpError && reason.status === 401
          ? "That code didn't work. Codes expire quickly; check Zed for a fresh one."
          : reason instanceof TypeError
            ? "Couldn't reach Zed. Make sure this phone can reach your computer."
            : errorMessage(reason),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="pairing">
      <form class="pairing-card" onSubmit={submit}>
        <div class="pairing-logo">
          <img src="/icons/icon-192.png" alt="" width="72" height="72" />
        </div>
        <h1>Pair with Zed</h1>
        <p class="pairing-lead">
          To get a code, run <b>web ui: pair device</b> from the command palette in Zed on your desktop. Enter it here
          to control Zed from this device.
        </p>
        {revoked && (
          <div class="callout error compact">
            <Icon name="alert" size={16} />
            <div class="callout-text">Zed no longer accepts this device, e.g. because its HTTP API secret changed. Pair it again to reconnect.</div>
          </div>
        )}
        <label class="field">
          <span class="field-label">Pairing code</span>
          <input
            class="code-input"
            type="text"
            inputMode="text"
            autocomplete="one-time-code"
            autocapitalize="characters"
            autocorrect="off"
            spellcheck={false}
            placeholder="XXXX-XXXX"
            maxLength={9}
            value={code}
            onInput={(event) => {
              const formatted = formatCode(event.currentTarget.value);
              event.currentTarget.value = formatted;
              setCode(formatted);
            }}
          />
        </label>
        <label class="field">
          <span class="field-label">Device name</span>
          <input
            type="text"
            autocomplete="off"
            value={name}
            maxLength={60}
            onInput={(event) => setName(event.currentTarget.value)}
          />
        </label>
        {error && <div class="field-error">{error}</div>}
        <button type="submit" class="button primary large pressable" disabled={!complete || busy}>
          {busy ? <Spinner size={18} /> : null}
          {busy ? "Pairing…" : "Pair"}
        </button>
        {isIOS && !isStandalone() && (
          <p class="pairing-hint">
            <Icon name="info" size={14} />
            For notifications, first add Zed to your Home Screen (Share → Add to Home Screen) and pair from there; the
            installed app keeps its own sign-in.
          </p>
        )}
      </form>
    </div>
  );
}
