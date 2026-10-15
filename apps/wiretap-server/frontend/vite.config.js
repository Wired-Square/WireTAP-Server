import { readFileSync } from "node:fs";
import { defineConfig } from "vite";
import { applianceProxy, applianceVite } from "@wired-square/appliance-ui/vite";

const api = process.env.WIRETAP_SERVER_UI_API ?? "https://localhost:8443";

// The standalone UI's session cookie is `Secure`, so the dev server needs its
// https too: the pair the UI mints in `apps/wiretap-server/`.
const minted = () => {
  const read = (name) => readFileSync(new URL(`../${name}`, import.meta.url));
  try {
    return { cert: read("cert.pem"), key: read("key.pem") };
  } catch {
    return undefined;
  }
};

export default defineConfig(({ command }) =>
  applianceVite({
    server: {
      proxy: applianceProxy(api),
      ...(command === "serve" && api.startsWith("https:") && { https: minted() }),
    },
  }),
);
