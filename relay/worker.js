/**
 * BAF Notification Relay — Cloudflare Worker
 *
 * Receives HMAC-signed JSON from every bot instance and forwards the
 * appropriate Discord embed to the central webhook.  The Discord webhook URL
 * never appears in the bot source code or in any user's config file.
 *
 * ─── Setup (one-time) ────────────────────────────────────────────────────────
 *
 * 1. Install Wrangler (Cloudflare's CLI) if you haven't already:
 *      npm install -g wrangler
 *      wrangler login
 *
 * 2. Deploy the worker from the relay/ directory:
 *      cd relay
 *      npx wrangler deploy
 *
 *    NOTE: Do NOT upload worker.js via the Cloudflare Dashboard file uploader —
 *    it does not support ES module workers.  Use `wrangler deploy` as above.
 *
 * 3. Add two encrypted Worker Secrets (run these after the first deploy):
 *      npx wrangler secret put DISCORD_WEBHOOK_URL
 *      npx wrangler secret put BAF_NOTIFY_SECRET
 *
 *    DISCORD_WEBHOOK_URL   — the full Discord webhook URL for your channel
 *    BAF_NOTIFY_SECRET     — any random string (≥ 32 chars); must match the
 *                            value you set in the GitHub Actions secret of the
 *                            same name so the CI bakes it into every binary.
 *
 * 4. Copy the Worker URL (e.g. https://baf-relay.yourname.workers.dev) and add
 *    it as a GitHub Actions secret named  BAF_NOTIFY_RELAY_URL.  The CI will
 *    bake it into every released binary via option_env!().
 *
 * ─── Request format ──────────────────────────────────────────────────────────
 *
 * POST /
 * Content-Type: application/json
 *
 * {
 *   "event":     "legendary_flip" | "divine_flip" | "legendary_bazaar_flip"
 *                | "divine_bazaar_flip" | "ban_notify",
 *   "timestamp": <unix seconds>,
 *   "payload":   { ... event-specific fields ... },
 *   "signature": "<hex HMAC-SHA256 of 'event:timestamp:payload_json'>"
 * }
 *
 * ─── Security ────────────────────────────────────────────────────────────────
 *
 * - Requests without a valid signature are rejected with 401.
 * - Timestamps older than 5 minutes are rejected with 401 (replay prevention).
 * - The Discord webhook URL is never exposed to callers.
 */

export default {
  async fetch(request, env) {
    if (request.method !== "POST") {
      return new Response("Method Not Allowed", { status: 405 });
    }

    // ── Parse body ────────────────────────────────────────────────────────────
    let body;
    try {
      body = await request.json();
    } catch {
      return new Response("Bad Request: invalid JSON", { status: 400 });
    }

    const { event, timestamp, payload, signature } = body;
    if (!event || !timestamp || !payload) {
      return new Response("Bad Request: missing fields", { status: 400 });
    }

    // ── Verify HMAC signature ────────────────────────────────────────────────
    if (!env.BAF_NOTIFY_SECRET) {
      // Secret not configured — refuse all requests so the relay is never open.
      return new Response("Relay not configured", { status: 503 });
    }

    const payloadJson = JSON.stringify(payload);
    const message = `${event}:${timestamp}:${payloadJson}`;
    const expectedSig = await hmacSha256Hex(env.BAF_NOTIFY_SECRET, message);

    if (!signature || !timingSafeEqual(signature, expectedSig)) {
      return new Response("Unauthorized", { status: 401 });
    }

    // ── Replay protection (5-minute window) ──────────────────────────────────
    const nowSecs = Math.floor(Date.now() / 1000);
    if (Math.abs(nowSecs - Number(timestamp)) > 300) {
      return new Response("Unauthorized: timestamp out of range", { status: 401 });
    }

    // ── Build Discord embed ───────────────────────────────────────────────────
    const discordBody = buildDiscordPayload(event, payload);
    if (!discordBody) {
      // Unknown event — ignore silently (forward-compat with future bot versions).
      return new Response("OK", { status: 200 });
    }

    // ── Forward to Discord ────────────────────────────────────────────────────
    if (!env.DISCORD_WEBHOOK_URL) {
      return new Response("Relay not configured", { status: 503 });
    }

    const discordRes = await fetch(env.DISCORD_WEBHOOK_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(discordBody),
    });

    if (!discordRes.ok) {
      const text = await discordRes.text();
      console.error(`Discord returned ${discordRes.status}: ${text}`);
      return new Response("Bad Gateway", { status: 502 });
    }

    return new Response("OK", { status: 200 });
  },
};

// ── Discord embed builders ────────────────────────────────────────────────────

function buildDiscordPayload(event, payload) {
  switch (event) {
    case "legendary_flip":
      return buildFlipEmbed(payload, {
        title: "🌟 Legendary Flip!",
        color: 0xffd700,
      });
    case "divine_flip":
      return buildFlipEmbed(payload, {
        title: "💎 Divine Flip!",
        color: 0x00ffff,
      });
    case "legendary_bazaar_flip":
      return buildBazaarFlipEmbed(payload, {
        title: "🌟 Legendary Bazaar Flip!",
        color: 0xffd700,
      });
    case "divine_bazaar_flip":
      return buildBazaarFlipEmbed(payload, {
        title: "💎 Divine Bazaar Flip!",
        color: 0x00ffff,
      });
    case "ban_notify":
      return {
        embeds: [
          {
            title: "⛔ Ban Detected",
            description: "A user of this macro just got banned.",
            color: 0xe74c3c,
          },
        ],
      };
    default:
      return null;
  }
}

function buildFlipEmbed(payload, { title, color }) {
  const { item_name, price, target, profit, buy_speed_ms, finder } = payload;
  const fields = [];

  fields.push({
    name: "💰 Purchase Price",
    value: `\`\`\`fix\n${formatNumber(price)} coins\n\`\`\``,
    inline: true,
  });
  if (target != null) {
    fields.push({
      name: "🎯 Target Price",
      value: `\`\`\`fix\n${formatNumber(target)} coins\n\`\`\``,
      inline: true,
    });
  }
  if (profit != null) {
    const sign = profit >= 0 ? "+" : "";
    fields.push({
      name: "📈 Expected Profit",
      value: `\`\`\`diff\n${sign}${formatNumber(profit)} coins\n\`\`\``,
      inline: true,
    });
  }
  if (buy_speed_ms != null) {
    fields.push({
      name: "⚡ Buy Speed",
      value: `\`\`\`\n${buy_speed_ms}ms\n\`\`\``,
      inline: true,
    });
  }
  if (finder) {
    fields.push({
      name: "🔍 Finder",
      value: `\`\`\`\n${toTitleCase(finder)}\n\`\`\``,
      inline: true,
    });
  }

  const safeItem = sanitizeItemName(item_name ?? "");
  return {
    embeds: [
      {
        title,
        description: item_name ?? "Unknown item",
        color,
        fields,
        thumbnail: {
          url: `https://sky.coflnet.com/static/icon/${safeItem}`,
        },
        footer: { text: "BAF public channel" },
      },
    ],
  };
}

function buildBazaarFlipEmbed(payload, { title, color }) {
  const { item_name, amount, price_per_unit, total, profit } = payload;
  const fields = [];

  if (amount != null) {
    fields.push({
      name: "📦 Amount",
      value: `\`\`\`fix\n${amount}x\n\`\`\``,
      inline: true,
    });
  }
  if (price_per_unit != null) {
    fields.push({
      name: "💵 Price/Unit",
      value: `\`\`\`fix\n${formatNumber(price_per_unit)} coins\n\`\`\``,
      inline: true,
    });
  }
  if (total != null) {
    fields.push({
      name: "💰 Total",
      value: `\`\`\`fix\n${formatNumber(total)} coins\n\`\`\``,
      inline: true,
    });
  }
  if (profit != null) {
    const sign = profit >= 0 ? "+" : "";
    fields.push({
      name: "📈 Profit",
      value: `\`\`\`diff\n${sign}${formatNumber(profit)} coins\n\`\`\``,
      inline: true,
    });
  }

  const safeItem = sanitizeItemName(item_name ?? "");
  return {
    embeds: [
      {
        title,
        description: item_name ?? "Unknown item",
        color,
        fields,
        thumbnail: {
          url: `https://sky.coflnet.com/static/icon/${safeItem}`,
        },
        footer: { text: "BAF public channel" },
      },
    ],
  };
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function formatNumber(n) {
  n = Number(n);
  // Numeric separator literals (_) are ES2021; Cloudflare Workers' V8 engine
  // has supported them since 2019, so this is safe for this target runtime.
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(2)}K`;
  return String(Math.round(n));
}

function sanitizeItemName(name) {
  return name
    .split("")
    .map((c) => (/[a-zA-Z0-9]/.test(c) ? c.toUpperCase() : "_"))
    .join("")
    .replace(/^_+|_+$/g, "");
}

function toTitleCase(s) {
  return s
    .split("_")
    .map((w) => w.charAt(0).toUpperCase() + w.slice(1).toLowerCase())
    .join(" ");
}

/** Compute HMAC-SHA256 over `message` using `key`, return hex string. */
async function hmacSha256Hex(key, message) {
  const enc = new TextEncoder();
  const cryptoKey = await crypto.subtle.importKey(
    "raw",
    enc.encode(key),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"]
  );
  const sig = await crypto.subtle.sign("HMAC", cryptoKey, enc.encode(message));
  return Array.from(new Uint8Array(sig))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** Constant-time string comparison to prevent timing attacks. */
function timingSafeEqual(a, b) {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) {
    diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  }
  return diff === 0;
}
