import { resolve } from "node:path";
import { defineConfig } from "vite";

export default defineConfig({
  build: {
    rollupOptions: {
      input: {
        index: resolve(import.meta.dirname, "index.html"),
        gettingStarted: resolve(import.meta.dirname, "getting-started.html"),
        sdk: resolve(import.meta.dirname, "sdk.html"),
        architecture: resolve(import.meta.dirname, "architecture.html"),
      },
    },
  },
});
