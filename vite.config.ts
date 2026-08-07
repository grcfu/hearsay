import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri serves the frontend from this dev server and expects a fixed port; if the port
// is taken we want to know rather than have Tauri quietly point at nothing.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // These are Rust and Python territory; watching them just restarts Vite for no
      // reason while a Rust build is already in progress.
      ignored: ["**/src-tauri/**", "**/python/**", "**/helper/**", "**/target/**"],
    },
  },
  build: {
    target: "safari15",
    sourcemap: true,
  },
});
