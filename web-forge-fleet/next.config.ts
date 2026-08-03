import type { NextConfig } from "next";

const GATEWAY = process.env.FF_GATEWAY_URL ?? "http://127.0.0.1:8787";

const nextConfig: NextConfig = {
  // Static export — ff-gateway serves web-forge-fleet/out as SPA statics,
  // same deployment model as the old Vite dashboard (no Node server).
  output: "export",
  images: { unoptimized: true },
  // Directory-style URLs (/fleet/ -> out/fleet/index.html) so any static
  // file server with an index.html fallback serves routes correctly.
  trailingSlash: true,

  // Dev-only proxy: `next dev` forwards API calls to ff-gateway.
  // Ignored by `output: export` builds. WebSocket (/ws) and SSE
  // (/api/events/stream) connect directly to the gateway in dev — see
  // lib/gateway.ts.
  async rewrites() {
    return [
      { source: "/api/:path*", destination: `${GATEWAY}/api/:path*` },
      { source: "/v1/:path*", destination: `${GATEWAY}/v1/:path*` },
      { source: "/mcp/:path*", destination: `${GATEWAY}/mcp/:path*` },
      { source: "/slm/:path*", destination: `${GATEWAY}/slm/:path*` },
    ];
  },
};

export default nextConfig;
