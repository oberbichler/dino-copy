import { defineConfig, devices } from "@playwright/test";

// The window runs in WebKit, the engine of macOS's WKWebView. It is served
// from the production build (dist/), so build first: `npm run test:ui` does.
export default defineConfig({
  testDir: "tests",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    ...devices["Desktop Safari"],
    baseURL: "http://localhost:4173",
    viewport: { width: 520, height: 320 },
    deviceScaleFactor: 2,
    screenshot: "only-on-failure",
  },
  projects: [
    { name: "light", use: { colorScheme: "light" } },
    { name: "dark", use: { colorScheme: "dark" } },
  ],
  webServer: {
    command: "npx vite preview --port 4173 --strictPort",
    url: "http://localhost:4173",
    reuseExistingServer: !process.env.CI,
  },
});
