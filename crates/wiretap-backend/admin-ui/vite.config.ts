import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react()],
  // Relative, so the bundle works under any base path the gateway is given.
  base: "./",
  server: {
    // Dev convenience: proxy API calls to a locally running backend
    proxy: { "/v1": "http://localhost:8423" },
  },
});
