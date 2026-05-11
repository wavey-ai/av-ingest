const UI_PREFIX = "/code/av-ingest";

export default {
  async fetch(request, env) {
    if (request.method === "OPTIONS") {
      return new Response(null, { status: 204, headers: corsHeaders() });
    }

    const url = new URL(request.url);
    if (url.pathname === UI_PREFIX) {
      return Response.redirect(`${url.origin}${UI_PREFIX}/${url.search}`, 308);
    }

    if (!url.pathname.startsWith(`${UI_PREFIX}/`)) {
      return new Response("Not found\n", { status: 404, headers: corsHeaders() });
    }

    url.pathname = url.pathname.slice(UI_PREFIX.length) || "/";
    return env.ASSETS.fetch(new Request(url, request));
  },
};

function corsHeaders() {
  return {
    "Access-Control-Allow-Origin": "*",
    "Access-Control-Allow-Methods": "GET, HEAD, OPTIONS",
    "Access-Control-Allow-Headers": "Content-Type",
  };
}
