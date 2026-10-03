import { readFileSync } from "node:fs";
import { defineConfig } from "vite";
import { applianceProxy, applianceVite } from "@wired-square/appliance-ui/vite";

// `npm run dev` proxies the API to a daemon started from `appliance/`, and
// serves https with the pair that daemon mints there: the session cookie is
// `Secure`, and a browser drops it on a plaintext origin.
const daemon = process.env.WIRETAP_APPLIANCE_API ?? "https://localhost:8443";
const pem = (name) => readFileSync(new URL(`../${name}`, import.meta.url));

export default defineConfig(({ command }) =>
  applianceVite({
    server: {
      proxy: applianceProxy(daemon),
      ...(command === "serve" && { https: { cert: pem("cert.pem"), key: pem("key.pem") } }),
    },
  }),
);
