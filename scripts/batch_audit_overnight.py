#!/usr/bin/env python3
import os
import sys
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
LOG_FILE = "/root/proyectos/openproxy/scripts/batch_audit.log"
RESULTS_FILE = "/root/proyectos/openproxy/scripts/overnight_audit_results.json"

def log(msg):
    ts = time.strftime("%Y-%m-%d %H:%M:%S")
    line = f"[{ts}] {msg}"
    print(line, flush=True)
    with open(LOG_FILE, "a") as f:
        f.write(line + "\n")

def get_admin_key():
    with open(CONFIG_PATH, "r") as f:
        m = re.search(r"^\s*api_key:\s*(\S+)", f.read(), re.MULTILINE)
        if m:
            return m.group(1)
    raise ValueError("api_key not found")

def get_active_models(provider_id):
    conn = sqlite3.connect(DB_PATH)
    cur = conn.cursor()
    cur.execute("SELECT id, model_id FROM models WHERE provider_id=? AND active=1", (provider_id,))
    rows = cur.fetchall()
    conn.close()
    return [{"id": r[0], "model_id": r[1]} for r in rows]

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
            pred = analysis.get("prediction", "unknown")
            fam = analysis.get("family_prediction", "unknown")
            score = round(analysis.get("ranking_score", 0), 3)
            num_count = sample.get("count", 0)
            ttft = round(sample.get("throughput", {}).get("ttftMs", 0) / 1000.0, 2)
            tps = round(sample.get("throughput", {}).get("tokensPerSecond", 0), 1)
            raw_nums = sample.get("text", "")
            return {
                "status": "VERIFIED",
                "prediction": pred,
                "family": fam,
                "score": score,
                "numbers_parsed": num_count,
                "ttft_s": ttft,
                "tps": tps,
                "duration_s": dur,
                "sample_numbers": raw_nums[:150] + ("..." if len(raw_nums) > 150 else "")
            }
        elif data:
            sample = data["rounds"][0]["samples"][0] if data.get("rounds") and data["rounds"][0].get("samples") else {}
            err = sample.get("error") or data.get("rounds", [{}])[0].get("error") or "Unknown failure"
            preview = (sample.get("rawText") or sample.get("text") or "")[:150].replace("\n", " ")
            return {
                "status": "REJECTED_OR_INSUFFICIENT",
                "numbers_parsed": sample.get("count", 0),
                "duration_s": dur,
                "error": err,
                "preview": preview
            }
        else:
            return {
                "status": "CLI_ERROR_NO_JSON",
                "duration_s": dur,
                "error": proc.stderr[:200].strip() or "No output"
            }
    except subprocess.TimeoutExpired:
        return {
            "status": "GLOBAL_TIMEOUT",
            "duration_s": timeout_s + 30,
            "error": f"Timeout {timeout_s + 30}s exceeded"
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
    admin_key = get_admin_key()
    providers = ["jerouter", "tiprouter", "xkiro"]
    
    # Cargar o inicializar estado existente para soportar reanudación
    all_results = {}
    if os.path.exists(RESULTS_FILE):
        try:
            with open(RESULTS_FILE, "r") as f:
                all_results = json.load(f)
        except Exception:
            all_results = {}

    log("=================================================================")
    log(" INICIANDO AUDITORÍA AUTOMÁTICA NOCTURNA: JEROUTER, TIPROUTER, XKIRO")
    log("=================================================================")

    for prov in providers:
        models = get_active_models(prov)
        log(f"\n--- INICIANDO PROVEEDOR: {prov} ({len(models)} modelos activos) ---")
        if prov not in all_results:
            all_results[prov] = {}

        for idx, m in enumerate(models, 1):
            mid = m["model_id"]
            db_id = m["id"]
            
            # Si ya fue auditado en esta corrida y verificado, saltar
            if mid in all_results[prov] and all_results[prov][mid].get("status") == "VERIFIED":
                log(f"[{idx}/{len(models)}] {prov}/{mid} -> Ya verificado previamente, omitiendo.")
                continue

            log(f"[{idx}/{len(models)}] Auditando {prov}/{mid} (db_id: {db_id}) ...")
            
            # Ejecutar LMFPD con 180s timeout
            res = run_lmfpd(prov, mid, admin_key, timeout_s=180)
            res["id"] = db_id
            all_results[prov][mid] = res

            # Guardar progreso incremental
            with open(RESULTS_FILE, "w") as f:
                json.dump(all_results, f, indent=2)

            status = res["status"]
            if status == "VERIFIED":
                log(f"   ✓ ÉXITO: {res['numbers_parsed']} números extraídos | Predicción: {res['prediction']} ({res['family']}) [score: {res['score']}] | TTFT: {res['ttft_s']}s | Duración: {res['duration_s']}s")
            elif status == "REJECTED_OR_INSUFFICIENT":
                log(f"   ✗ FALLÓ: {res['error']} | Números: {res.get('numbers_parsed', 0)} | Duración: {res['duration_s']}s")
                if res.get("preview"):
                    log(f"     Preview: \"{res['preview']}\"")
                # Desactivar modelo fallido
                ok = toggle_model_api(db_id, admin_key)
                log(f"     -> Desactivación vía API: {'EXITOSA (active=0)' if ok else 'ERROR'}")
            else:
                log(f"   ⚠ ERROR/TIMEOUT: {res['error']} | Duración: {res['duration_s']}s")
                ok = toggle_model_api(db_id, admin_key)
                log(f"     -> Desactivación vía API: {'EXITOSA (active=0)' if ok else 'ERROR'}")

            # Pequeña pausa para no saturar upstream con 429
            time.sleep(3)

    log("\n=================================================================")
    log(" AUDITORÍA NOCTURNA COMPLETADA EXITOSAMENTE PARA TODOS LOS PROVEEDORES")
    log("=================================================================")

if __name__ == "__main__":
    main()
