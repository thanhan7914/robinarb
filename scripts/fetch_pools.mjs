// Fetch Base pools from DEX Screener, filter to supported DEXes, emit pools.toml.
//
//   node scripts/fetch_pools.mjs > pools.toml
//   TOP=1000 MIN_LIQ=2000 TOKENS=800 node scripts/fetch_pools.mjs > pools.toml
//   EXTRA="AIXBT,0xpool...,0xtoken..." node scripts/fetch_pools.mjs > pools.toml
//
// Needs Node >= 18 (global fetch). No RPC needed — pure HTTP API.
//
// Crawl seeds (WETH/USDC/cbBTC) -> their pools -> counterpart tokens -> their
// pools. Counterpart ranking uses liquidity + 24h VOLUME, so trending/"hot"
// tokens (high volume, thin liquidity — the ones on the DexScreener front page)
// get pulled in, not just deep-liquidity pairs. A pool survives the final cut
// if it clears MIN_LIQ *or* VOL_MIN. Selection favors pairs quoted on >= 2
// venues (arb sources).
//
// WHY A HOT POOL CAN BE MISSING: the crawl only reaches a token if it's paired
// with a seed or an already-crawled token. A brand-new token paired only with
// obscure tokens is unreachable. Fix: pass it via EXTRA (symbol / token addr /
// pool addr — anything DexScreener search accepts); EXTRA pools bypass every
// filter and are always included.
//
// NOTE: the loader classifies pools by on-chain factory() and drops unknown
// DEXes, so mislabeled entries here are harmless (they get dropped at load).

const CHAIN = "robinhood"; // confirmed exact slug, docs/PHASE0_LOOKUPS.md §8
const WETH = "0x4200000000000000000000000000000000000006";
const USDC = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const CBBTC = "0xcbB7C0000aB88B473b1f5aFd9ef808440eed33Bf";

const TOP = Number(process.env.TOP || 200);
const MIN_LIQ = Number(process.env.MIN_LIQ || 30000);
const VOL_MIN = Number(process.env.VOL_MIN || MIN_LIQ * 5); // 24h vol floor for hot pools
const TOKENS = Number(process.env.TOKENS || 30); // phase-2 crawl breadth
const EXTRA = (process.env.EXTRA || "").split(",").map((s) => s.trim()).filter(Boolean);

// Map DEX Screener (dexId, labels) -> our config [[dex]] name (best-effort hint).
function mapDex(dexId, labels) {
  const L = (labels || []).map((s) => String(s).toLowerCase());
  // Uniswap V4: pairAddress is the bytes32 poolId (no pool contract). The pin
  // is emitted as pool_id; the loader resolves it via the V4 discovery cache.
  if (L.includes("v4")) return dexId === "uniswap" ? "uniswap_v4" : null;
  const isV3 = L.includes("v3");
  const isCl = L.some((s) => s.includes("cl") || s.includes("slipstream") || s.includes("concentrated"));
  switch (dexId) {
    case "uniswap":
      return isV3 ? "uniswap_v3" : "uniswap_v2";
    case "aerodrome":
    case "aerodrome-slipstream":
      return isV3 || isCl || dexId === "aerodrome-slipstream" ? "slipstream" : "aerodrome";
    case "pancakeswap":
      return isV3 ? "pancakeswap_v3" : "pancakeswap_v2";
    case "sushiswap":
      return isV3 ? null : "sushiswap_v2";
    case "alien-base":
      return isV3 ? "alien_base_v3" : null;
    case "hydrex":
      return "hydrex"; // Algebra Integral (label "CLAMM")
    case "quickswap":
      // v4 label = Algebra Integral; v2 = a distinct UniV2 fork we don't track.
      return L.includes("v4") ? "quickswap_v4" : null;
    default:
      return null;
  }
}

async function tokenPairs(token) {
  const url = `https://api.dexscreener.com/token-pairs/v1/${CHAIN}/${token}`;
  const r = await fetch(url, { headers: { accept: "application/json" } });
  if (!r.ok) throw new Error(`${url} -> ${r.status}`);
  return r.json();
}

// Search accepts a symbol, token address, or pool address and returns pairs.
async function search(q) {
  const url = `https://api.dexscreener.com/latest/dex/search?q=${encodeURIComponent(q)}`;
  const r = await fetch(url, { headers: { accept: "application/json" } });
  if (!r.ok) throw new Error(`${url} -> ${r.status}`);
  const j = await r.json();
  return (j.pairs || []).filter((p) => p.chainId === CHAIN);
}

const vol24 = (p) => (p.volume && p.volume.h24) || 0;
const liqUsd = (p) => (p.liquidity && p.liquidity.usd) || 0;
const sleep = (ms) => new Promise((res) => setTimeout(res, ms));

async function crawl(tokens, sink) {
  for (const t of tokens) {
    try {
      sink.push(...(await tokenPairs(t)));
    } catch (e) {
      console.error("fetch failed", t, e.message);
    }
    await sleep(250); // stay far under the 300 req/min limit
  }
}

// Phase 1: seed pools.
const seed = [WETH, USDC, CBBTC];
const all = [];
await crawl(seed, all);

// Phase 2+: BFS over counterpart tokens ranked by liquidity + 24h volume, so
// trending tokens (hot but thin) get crawled alongside deep-liquidity ones.
const visited = new Set(seed.map((s) => s.toLowerCase()));
while (visited.size < TOKENS + seed.length) {
  const tokenScore = new Map(); // address -> { score, sym }
  for (const p of all) {
    const liq = liqUsd(p);
    const vol = vol24(p);
    if (liq < MIN_LIQ / 2 && vol < VOL_MIN) continue; // deep OR hot
    for (const t of [p.baseToken, p.quoteToken]) {
      const a = (t.address || "").toLowerCase();
      if (!a || visited.has(a)) continue;
      const e = tokenScore.get(a) || { score: 0, sym: t.symbol };
      e.score += liq + vol; // volume pulls in trending tokens
      tokenScore.set(a, e);
    }
  }
  const next = [...tokenScore.entries()]
    .sort((a, b) => b[1].score - a[1].score)
    .slice(0, TOKENS + seed.length - visited.size);
  if (next.length === 0) break;
  console.error("crawling:", next.map(([, e]) => e.sym).join(" "));
  for (const [a] of next) visited.add(a);
  await crawl(next.map(([a]) => a), all);
}

// EXTRA: force-include hot pools/tokens copied from the DexScreener website.
// Accepts symbol / token address / pool address. These bypass every filter.
const forced = new Set();
for (const q of EXTRA) {
  try {
    const pairs = await search(q);
    for (const p of pairs) {
      all.push(p);
      if (/^0x[0-9a-fA-F]{40}$/.test(p.pairAddress)) forced.add(p.pairAddress.toLowerCase());
    }
    console.error(`EXTRA "${q}": +${pairs.length} pairs`);
    await sleep(250);
  } catch (e) {
    console.error(`EXTRA "${q}" failed:`, e.message);
  }
}

// Normalize + dedupe by pool address.
const byAddr = new Map();
for (const p of all) {
  const dex = mapDex(p.dexId, p.labels);
  if (!dex) continue;
  const addr = p.pairAddress;
  const isV4 = dex === "uniswap_v4";
  // V4 ids are bytes32; everything else must be a 20-byte pool address.
  if (isV4 && !/^0x[0-9a-fA-F]{64}$/.test(addr)) continue;
  if (!isV4 && !/^0x[0-9a-fA-F]{40}$/.test(addr)) continue;
  const liq = liqUsd(p);
  const vol = vol24(p);
  const isForced = forced.has(addr.toLowerCase());
  // Keep if deep OR hot OR explicitly forced via EXTRA.
  if (!isForced && liq < MIN_LIQ && vol < VOL_MIN) continue;
  const pairKey = [p.baseToken.address.toLowerCase(), p.quoteToken.address.toLowerCase()].sort().join("-");
  if (!byAddr.has(addr) || byAddr.get(addr).liq < liq) {
    byAddr.set(addr, { addr, dex, liq, vol, pairKey, forced: isForced, sym: `${p.baseToken.symbol}/${p.quoteToken.symbol}` });
  }
}

// Selection: pools whose token pair is quoted on >= 2 venues first (arb
// sources), then top-up by raw liquidity.
const byPair = new Map();
for (const r of byAddr.values()) {
  (byPair.get(r.pairKey) || byPair.set(r.pairKey, []).get(r.pairKey)).push(r);
}
const forcedRows = [];
const multi = [];
const single = [];
for (const group of byPair.values()) {
  for (const r of group) {
    if (r.forced) forcedRows.push(r);
    else (group.length >= 2 ? multi : single).push(r);
  }
}
const byScore = (a, b) => b.liq + b.vol - (a.liq + a.vol);
multi.sort(byScore);
single.sort(byScore);
// Forced (EXTRA) pools always kept; fill the rest up to TOP.
const rest = [...multi, ...single].slice(0, Math.max(0, TOP - forcedRows.length));
const rows = [...forcedRows, ...rest];
console.error(
  `selected: ${rows.length} (${forcedRows.length} forced, ${multi.length} on multi-venue pairs, ${byPair.size} distinct pairs)`
);

let out =
  "# High-liquidity Base pools from DEX Screener (WETH/USDC/cbBTC seeds + 1-level token crawl).\n" +
  "# Used when [discovery].enabled = false. Regenerate with scripts/fetch_pools.mjs.\n" +
  "# The loader re-classifies by on-chain factory() and drops unknown DEXes.\n" +
  "# Uniswap V4 pins use pool_id (bytes32) — resolving them requires the V4\n" +
  "# discovery cache (run `discover` with the uniswap_v4 [[dex]] enabled once).\n\n";
for (const r of rows) {
  const tag = r.forced ? " [EXTRA]" : "";
  out += `[[pool]]\n# ${r.sym}${tag} — $${Math.round(r.liq).toLocaleString()} liq, $${Math.round(r.vol).toLocaleString()} vol24\n`;
  if (r.dex === "uniswap_v4") {
    out += `pool_id = "${r.addr}"\ndex = "${r.dex}"\n\n`;
  } else {
    out += `address = "${r.addr}"\ndex = "${r.dex}"\n\n`;
  }
}
process.stdout.write(out);
