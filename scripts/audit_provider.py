#!/usr/bin/env python3
import sys
import os
import argparse
import subprocess
import json
import sqlite3
import re
import time
import requests

DB_PATH = "/root/.openproxy/data.db"
CONFIG_PATH = "/root/.hermes/config.yaml"
BASE_URL = "http://127.0.0.1:8787/v1"
ADMIN_TOGGLE_URL = "http://127.0.0.1:8787/admin/api/models/{id}/toggle"

def get_admin_key():
    if not os.path.exists(CONFIG_PATH):
        raise FileNotFoundError(f"Config not found at {CONFIG_PATH}")
    with open(CONFIG_PATH, "r") as f:
        m = re.search(r"^\s*api_key:\s*(\S+)", f.read(), re.MULTILINE)
        if m:
            return m.group(1)
    raise ValueError("api_key not found in config.yaml")

def get_models(provider_id, active_only=True):
    conn = sqlite3.connect(DB_PATH)
    cur = conn.cursor()
    query = "SELECT id, model_id, active FROM models WHERE provider_id=?"
    if active_only:
        query += " AND active=1"
    cur.execute(query, (provider_id,))
    rows = cur.fetchall()
    conn.close()
    return [{"id": r[0], "model_id": r[1], "active": r[2]} for r in rows]

def get_available_providers():
    conn = sqlite3.connect(DB_PATH)
    cur = conn.cursor()
    cur.execute("SELECT DISTINCT provider_id FROM models ORDER BY provider_id")
    rows = cur.fetchall()
    conn.close()
    return [r[0] for r in rows]

def run_lmfpd(provider_id, model_id, admin_key, timeout_s=180):
    full_model = f"{provider_id}/{model_id}"
    cmd = [
        "npx", "lmfpd",
        "-b", BASE_URL,
        "-k", admin_key,
        "-m", full_model,
        "-a", "cc",
        "-p", "1",
        "--count", "1",
        "--timeout", str(timeout_s),
        "--json"
    ]
    t0 = time.time()
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout_s + 30)
        dur = round(time.time() - t0, 2)
        out = proc.stdout.strip()
        data = None
        if out:
            idx = out.find('{')
            if idx != -1:
                try:
                    data = json.loads(out[idx:])
                except Exception:
                    pass

        if data and data.get("scored_rounds", 0) > 0:
            analysis = data["rounds"][0]["analysis"]
            sample = data["rounds"][0]["samples"][0]
            return {
                "status": "VERIFIED",
                "prediction": analysis.get("prediction", "unknown"),
                "family": analysis.get("family_prediction", "unknown"),
                "score": round(analysis.get("ranking_score", 0), 3),
                "numbers_parsed": sample.get("count", 0),
                "ttft_s": round(sample.get("throughput", {}).get("ttftMs", 0) / 1000.0, 2),
                "tps": round(sample.get("throughput", {}).get("tokensPerSecond", 0), 1),
                "duration_s": dur,
                "error": None
            }
        elif data:
            sample = data["rounds"][0]["samples"][0] if data.get("rounds") and data["rounds"][0].get("samples") else {}
            err = sample.get("error") or data.get("rounds", [{}])[0].get("error") or "Unknown failure"
            preview = (sample.get("rawText") or sample.get("text") or "")[:120].replace("\n", " ")
            return {
                "status": "REJECTED_OR_INSUFFICIENT",
                "prediction": None,
                "family": None,
                "score": None,
                "numbers_parsed": sample.get("count", 0),
                "duration_s": dur,
                "error": err,
                "preview": preview
            }
        else:
            return {
                "status": "TIMEOUT_OR_CLI_ERR",
                "duration_s": dur,
                "error": proc.stderr[:160].strip() or "No JSON output"
            }
    except subprocess.TimeoutExpired:
        return {
            "status": "TIMEOUT",
            "duration_s": timeout_s + 30,
            "error": f"Process exceeded {timeout_s + 30}s timeout"
        }
    except Exception as e:
        return {
            "status": "EXCEPTION",
            "duration_s": round(time.time() - t0, 2),
            "error": str(e)
        }

def toggle_model_api(model_db_id, admin_key):
    url = ADMIN_TOGGLE_URL.format(id=model_db_id)
    headers = {"Authorization": f"Bearer {admin_key}", "Content-Type": "application/json"}
    try:
        r = requests.post(url, headers=headers, json={"active": False}, timeout=10)
        return r.status_code == 200
    except Exception:
        return False

def main():
    parser = argparse.ArgumentParser(description="Auditoría automática de huellas (LMFPD) para proveedores OpenProxy")
    parser.add_argument("provider", nargs="?", help="ID del proveedor a auditar (ej. jerouter, xkiro, mwapi, nocta)")
    parser.add_argument("--all", action="store_true", help="Auditar todos los modelos sin importar su estado active")
    parser.add_argument("--deactivate-failed", action="store_true", help="Desactivar vía API admin los modelos que fallen la extracción")
    parser.add_argument("--timeout", type=int, default=180, help="Timeout por llamada lmfpd en segundos (default 180)")
    args = parser.parse_args()

    providers = get_available_providers()
    if not args.provider:
        print("\n=== PROVEEDORES DISPONIBLES EN OPENPROXY ===")
        for p in providers:
            print(f" - {p}")
        print("\nUso: python3 audit_provider.py <nombre_proveedor> [--deactivate-failed] [--timeout 180]")
        sys.exit(0)

    provider = args.provider
    if provider not in providers:
        print(f"Error: Proveedor '{provider}' no encontrado. Disponibles: {', '.join(providers)}")
        sys.exit(1)

    admin_key = get_admin_key()
    models = get_models(provider, active_only=not args.all)
    if not models:
        print(f"No hay modelos activos para el proveedor '{provider}'.")
        sys.exit(0)

    print(f"\n=======================================================")
    print(f" INICIANDO AUDITORÍA LMFPD: PROVEEDOR '{provider}'")
    print(f" Total modelos a auditar: {len(models)}")
    print(f" Desactivación automática: {'ACTIVADA' if args.deactivate_failed else 'DESACTIVADA'}")
    print(f"=======================================================\n")

    results = []
    for idx, m in enumerate(models, 1):
        mid = m["model_id"]
        db_id = m["id"]
        print(f"[{idx}/{len(models)}] Probando {provider}/{mid} ...", end=" ", flush=True)
        
        res = run_lmfpd(provider, mid, admin_key, timeout_s=args.timeout)
        res["id"] = db_id
        res["model_id"] = mid
        results.append(res)

        status = res["status"]
        if status == "VERIFIED":
            print(f"✓ VERIFICADO ({res['numbers_parsed']} números) -> {res['prediction']} ({res['family']}) [score: {res['score']}] - {res['duration_s']}s")
        elif status == "REJECTED_OR_INSUFFICIENT":
            print(f"✗ FALLÓ ({res['error']}) - {res['duration_s']}s")
            if res.get("preview"):
                print(f"    Respuesta: \"{res['preview']}\"")
            if args.deactivate_failed:
                ok = toggle_model_api(db_id, admin_key)
                print(f"    -> Desactivado vía API: {'OK' if ok else 'ERROR'}")
        else:
            print(f"⚠ ERROR/TIMEOUT ({res['error']}) - {res['duration_s']}s")
            if args.deactivate_failed:
                ok = toggle_model_api(db_id, admin_key)
                print(f"    -> Desactivado vía API: {'OK' if ok else 'ERROR'}")

    out_file = f"/root/proyectos/openproxy/scripts/audit_{provider}_report.json"
    with open(out_file, "w") as f:
        json.dump(results, f, indent=2)

    print(f"\nAuditoría completa. Reporte guardado en: {out_file}\n")

if __name__ == "__main__":
    main()
