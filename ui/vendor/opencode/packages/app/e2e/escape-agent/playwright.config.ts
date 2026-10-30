import { defineConfig, devices } from "@playwright/test"

// Live end-user checks against a running Escape Agent server. The suite does
// not start a fixture server: it exercises the same dist and API the user sees.
export default defineConfig({
  testDir: ".",
  timeout: 60_000,
  expect: { timeout: 10_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["line"], ["html", { outputFolder: "report", open: "never" }]],
  use: {
    baseURL: process.env.ESCAPE_BASE_URL ?? "http://127.0.0.1:4096",
    locale: "en-US",
    colorScheme: "dark",
    viewport: { width: 1440, height: 900 },
    serviceWorkers: "block",
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        launchOptions: process.env.ESCAPE_CHROMIUM
          ? { executablePath: process.env.ESCAPE_CHROMIUM }
          : undefined,
      },
    },
  ],
})
