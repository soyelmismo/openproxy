#!/usr/bin/env python3
"""Dry run del margin-router: para cada modelo objetivo, escanear sellers
anunciados en buyer.state.json, calcular el ratio oficial/comprado con los
perfiles de consumo y el margen por paquete ($5 -> $25 de crédito oficial).

Sellers elegibles: margen > 0 en el PEOR caso (95% hit) — regla "sí o sí".
Un modelo sale del index si no queda ningún seller elegible.
"""
import json
import statistics as st

BUYER_STATE = "/containers/config/antseed/data/buyer.state.json"
# Presupuesto del cliente: $5 pagados -> $25 de crédito a precios oficiales.
PACKAGE_USD = 5.0
CREDIT_USD = 25.0

# Oficiales (models.dev, medianas entre providers con coste; deepseek fijado por Miguel).
OFFICIAL = {
    "deepseek-v4.1-flash": (0.15, 0.003, 0.60),
    "claude-opus-5-5":     (4.0,  0.2,   20.0),
    "gpt-6.1-sol":         (2.0,  0.1,   10.0),
    "gpt-6-astra":         (10.0, 1.0,   50.0),
}

# Perfiles: (hit, out_share_of_input). El worst case es el que decide la elegibilidad.
PROFILES = [
    ("tu modelo 80%hit 95/5",  0.80, 0.05 / 0.95),
    ("real dealer 82.5% 0.6%", 0.825, 0.006),
    ("worst 95%hit 0.5%",      0.95, 0.005),
]

def peer_pricing(peer, slug):
    """(in, out, cached) del peer para el slug, o None. providerPricing.services o defaults."""
    pricing = peer.get("providerPricing") or {}
    services = peer.get("services") or []
    if slug not in services:
        return None
    for prov, pv in pricing.items():
        svcs = (pv or {}).get("services") or {}
        if slug in svcs:
            s = svcs[slug]
            return (s.get("inputUsdPerMillion"), s.get("outputUsdPerMillion"),
                    s.get("cachedInputUsdPerMillion"))
    # defaults del primer provider que anuncie defaults
    for prov, pv in pricing.items():
        d = (pv or {}).get("defaults") or {}
        if d.get("inputUsdPerMillion") is not None:
            return (d.get("inputUsdPerMillion"), d.get("outputUsdPerMillion"),
                    d.get("cachedInputUsdPerMillion"))
    return None

def mix_costs(official, seller, hit, out_share):
    """($ por 1M de input tokens) cliente vs nuestro, para el perfil dado."""
    of, oc, oo = official
    si, so, sc = seller
    client = (1 - hit) * of + hit * oc + out_share * oo
    ours = (1 - hit) * si + hit * sc + out_share * so
    return client, ours

def main():
    state = json.load(open(BUYER_STATE))
    peers = state["discoveredPeers"]
    for slug, official in OFFICIAL.items():
        rows = []
        for p in peers:
            price = peer_pricing(p, slug)
            if price is None:
                continue
            si, so, sc = price
            if not all(isinstance(x, (int, float)) and x >= 0 for x in (si, so, sc)):
                continue
            worst = mix_costs(official, price, *PROFILES[2][1:])
            client_w, ours_w = worst
            ratio_w = client_w / ours_w if ours_w > 0 else float("inf")
            cost_w = CREDIT_USD / ratio_w
            margin_w = PACKAGE_USD - cost_w
            real = mix_costs(official, price, *PROFILES[1][1:])
            ratio_r = real[0] / real[1] if real[1] > 0 else float("inf")
            margin_r = PACKAGE_USD - CREDIT_USD / ratio_r
            rows.append({
                "name": p.get("displayName") or p["peerId"][:12],
                "price": price, "rep": round(p.get("onChainReputationScore") or 0, 1),
                "maxc": p.get("maxConcurrency"),
                "wash": p.get("onChainWashFlagged"),
                "vol": round((p.get("onChainTotalVolumeUsdcMicros") or 0) / 1e6, 2),
                "ratio_w": ratio_w, "margin_w": margin_w,
                "ratio_r": ratio_r, "margin_r": margin_r,
                "eligible": margin_w > 0 and not p.get("onChainWashFlagged"),
            })
        rows.sort(key=lambda r: -r["margin_w"])
        elig = [r for r in rows if r["eligible"]]
        print(f"\n### {slug} — oficial {official[0]}/{official[1]}/{official[2]} — "
              f"{len(rows)} sellers anunciando, {len(elig)} ELEGIBLES")
        for r in rows:
            mark = "OK " if r["eligible"] else ("WASH" if r["wash"] else "out")
            print(f"  [{mark}] {r['name']:<28} in/out/cached {r['price'][0]:.4f}/{r['price'][1]:.4f}/{r['price'][2]:.4f} "
                  f"rep {r['rep']:5.1f} maxc {str(r['maxc']):>4} vol ${r['vol']:>8.2f} | "
                  f"real {r['ratio_r']:5.1f}x ${r['margin_r']:+6.2f} ({r['margin_r']/PACKAGE_USD*100:+5.0f}%) | "
                  f"worst {r['ratio_w']:5.1f}x ${r['margin_w']:+6.2f}")
        if elig:
            best, worst_e = elig[0], elig[-1]
            print(f"  -> INDEX: {len(elig)} sellers, orden por margen: "
                  + ", ".join(f"{r['name']} (${r['margin_w']:+.2f} worst)" for r in elig))
        else:
            print(f"  -> FUERA DEL INDEX: ningún seller da margen en el peor caso")

if __name__ == "__main__":
    main()
