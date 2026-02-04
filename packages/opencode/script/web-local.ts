#!/usr/bin/env bun

import { $ } from "bun"
import path from "path"
import { fileURLToPath } from "url"

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const root = path.resolve(__dirname, "../../..")
const appDir = path.join(root, "packages/app")
const distDir = path.join(appDir, "dist")
const opencode = path.join(root, "packages/opencode/dist/opencode-darwin-arm64/bin/opencode")

const APP_PORT = 5173
const forceBuild = process.argv.includes("--build")

// Check if app dist exists, build if not
const distExists = await Bun.file(path.join(distDir, "index.html")).exists()
if (!distExists || forceBuild) {
  console.log("Building app...")
  await $`bun run build`.cwd(appDir)
}

// Start static server for app
console.log(`Starting app server on http://127.0.0.1:${APP_PORT}`)
const server = Bun.serve({
  port: APP_PORT,
  hostname: "127.0.0.1",
  async fetch(req) {
    const url = new URL(req.url)
    let filepath = path.join(distDir, url.pathname)

    // Serve index.html for SPA routes
    const file = Bun.file(filepath)
    if (!(await file.exists()) || filepath.endsWith("/")) {
      filepath = path.join(distDir, "index.html")
    }

    const result = Bun.file(filepath)
    if (!(await result.exists())) {
      return new Response("Not Found", { status: 404 })
    }
    return new Response(result)
  },
})

console.log(`Starting opencode web...`)

// Run opencode web with local app URL
const proc = Bun.spawn([opencode, "web", "--hostname", "0.0.0.0"], {
  env: {
    ...process.env,
    OPENCODE_APP_URL: `http://127.0.0.1:${APP_PORT}`,
  },
  stdout: "inherit",
  stderr: "inherit",
  stdin: "inherit",
})

// Handle exit
process.on("SIGINT", () => {
  server.stop()
  proc.kill()
  process.exit(0)
})

await proc.exited
server.stop()
