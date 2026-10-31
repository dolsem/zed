import { defineConfig, type Plugin } from "vite";
import preact from "@preact/preset-vite";
import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { generateIcons } from "./scripts/icons.mjs";

// The real backend (the Rust `http_api` server, or `npm run mock`) to proxy
// `/api` to while running `npm run dev`.
const backend = process.env.ZED_MOBILE_BACKEND ?? "http://127.0.0.1:8787";

// Serves the generated PNG icons in dev and emits them into the build.
function icons(): Plugin {
  let files: Map<string, Buffer> | null = null;
  const all = () => (files ??= generateIcons());
  return {
    name: "zed-mobile-icons",
    configureServer(server) {
      server.middlewares.use((request, response, next) => {
        const path = (request.url ?? "").split("?")[0].replace(/^\//, "");
        const file = all().get(path);
        if (!file) return next();
        response.setHeader("Content-Type", "image/png");
        response.end(file);
      });
    },
    generateBundle() {
      for (const [fileName, source] of all()) {
        this.emitFile({ type: "asset", fileName, source });
      }
    },
  };
}

// Fills in the service worker's precache list and version after the build, so
// the app shell launches instantly and updates when any asset changes.
function serviceWorkerManifest(): Plugin {
  let outDir = "dist";
  const emitted: string[] = [];
  return {
    name: "zed-mobile-sw-manifest",
    apply: "build",
    configResolved(config) {
      outDir = resolve(config.root, config.build.outDir);
    },
    generateBundle(_options, bundle) {
      emitted.length = 0;
      for (const fileName of Object.keys(bundle)) {
        if (fileName.endsWith(".map")) continue;
        emitted.push("/" + fileName.replace(/\\/g, "/"));
      }
    },
    async closeBundle() {
      const swPath = resolve(outDir, "sw.js");
      const source = await readFile(swPath, "utf8");
      const precache = Array.from(
        new Set(["/", "/manifest.webmanifest", ...emitted.filter((file) => file !== "/index.html")]),
      ).sort();
      const version = createHash("sha256")
        .update(JSON.stringify(precache))
        .update(source)
        .digest("hex")
        .slice(0, 12);
      const patched = source
        .replace(/\/\*__PRECACHE__\*\/[\s\S]*?\/\*__END__\*\//, JSON.stringify(precache))
        .replace(/"__BUILD_VERSION__"/, JSON.stringify(version));
      await writeFile(swPath, patched);
    },
  };
}

export default defineConfig({
  plugins: [preact(), icons(), serviceWorkerManifest()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
    chunkSizeWarningLimit: 1500,
  },
  server: {
    host: true,
    proxy: {
      "/api": { target: backend, ws: true, changeOrigin: true },
    },
  },
});
