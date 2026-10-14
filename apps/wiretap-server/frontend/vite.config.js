import { readFileSync } from "node:fs";
import { defineConfig } from "vite";
import { applianceProxy, applianceVite } from "@wired-square/appliance-ui/vite";

// `npm run dev` proxies the API to a UI started from `apps/wiretap-server/`:
// standalone, over https with the pair it mints there, since the session
// cookie is `Secure`; or behind `appliance-xtask dev-console` with
// `WIRETAP_SERVER_UI_API=http://127.0.0.1:8080`.
const daemon = process.env.WIRETAP_SERVER_UI_API ?? "https://localhost:8443";
const pem = (name) => {
  try {
    return readFileSync(new URL(`../${name}`, import.meta.url));
  } catch {
    return undefined;
  }
};

export default defineConfig(({ command }) =>
  applianceVite({
    server: {
      proxy: applianceProxy(daemon),
      ...(command === "serve" &&
        daemon.startsWith("https:") &&
        pem("cert.pem") && { https: { cert: pem("cert.pem"), key: pem("key.pem") } }),
    },
  }),
);
