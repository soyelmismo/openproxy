# Backup and Restore Guide

This guide details how to export, validate, and restore your OpenProxy configuration, providers, accounts, credentials, and combos, as well as best practices for persistent storage on ephemeral hosting providers like Koyeb or Docker.

---

## 1. Overview

OpenProxy persists configuration, providers, models, combos, client API keys, and encrypted upstream credentials in SQLite.
The Backup & Restore system allows operators to:

- Export all database entities and runtime configuration into a single file named `openproxy-backup.json`.
- Safely migrate between environments, cloud platforms, or Docker container versions.
- Re-encrypt sensitive secrets (API keys, OAuth tokens) using the destination environment's `OPENPROXY_MASTER_KEY` upon restore.
- Create an automated safety backup of the database before applying changes.
- Choose between plain JSON (default, simple 1-click redeployment) or authenticated AES-256-GCM encryption with a user-defined passphrase.

---

## 2. Using the Dashboard UI

Navigate to **Config** in the OpenProxy Admin Dashboard and scroll to the **Backup & Restore** card.

### Exporting / Downloading a Backup
1. *(Optional)* Enter an encryption passphrase if you want the file to be sealed with AES-256-GCM. If left empty, the backup will be exported as unencrypted JSON.
2. Click **📦 Download Backup**.
3. Your browser will download `openproxy-backup.json`.
   > ⚠️ **Warning**: The backup file contains sensitive provider credentials and API keys. Store it securely and never commit it to public repositories.

### Restoring from a Backup
1. Click **Select Backup File** and choose your `openproxy-backup.json` file.
2. The UI detects whether the file is encrypted or plain JSON. If encrypted, a **Decryption Passphrase** field appears.
3. Click **🔍 Validate & Preview**. OpenProxy validates schema versioning and displays an inspection summary (number of providers, accounts, models, combos, and any warnings).
4. Click **⚠️ Restore Now** and confirm. A timestamped safety backup of the current database (e.g. `openproxy.db.safety-backup-YYYYMMDD_HHMMSS.db`) is automatically saved to disk before applying the restore in an atomic transaction.

---

## 3. CLI & API Automation

You can automate backup and restore in CI/CD, backup cron jobs, or container init scripts using `curl`.

### Export Backup

**Plain JSON:**
> Plain (unencrypted) export is opt-in only: the endpoint refuses it unless the query parameter `?plaintext=confirmed` is present, because the bundle contains every upstream credential in cleartext.
```bash
curl -s -H "Authorization: Bearer <ADMIN_KEY>" \
  "http://localhost:8787/admin/api/backup/export?plaintext=confirmed" \
  -o openproxy-backup.json
```

**Encrypted with Passphrase:**
> Send the passphrase in the `x-backup-passphrase` header (or in the restore/validate payload). It is never accepted as a query parameter: `?passphrase=...` is rejected with a validation error so the secret does not leak into access logs or shell history.
```bash
curl -s -H "Authorization: Bearer <ADMIN_KEY>" \
  -H "x-backup-passphrase: my-secret-passphrase" \
  "http://localhost:8787/admin/api/backup/export" \
  -o openproxy-backup.json
```

### Validate Backup

```bash
curl -X POST \
  -H "Authorization: Bearer <ADMIN_KEY>" \
  -H "Content-Type: application/json" \
  -d @openproxy-backup.json \
  http://localhost:8787/admin/api/backup/validate
```

*(If encrypted, supply the passphrase via `X-Backup-Passphrase: my-secret-passphrase` header or payload).*

### Restore Backup

```bash
curl -X POST \
  -H "Authorization: Bearer <ADMIN_KEY>" \
  -H "Content-Type: application/json" \
  -d @openproxy-backup.json \
  http://localhost:8787/admin/api/backup/restore
```

---

## 4. Ephemeral Deployments (Docker & Koyeb)

In ephemeral container environments, container filesystems are reset on redeployments or container restarts unless a persistent volume is attached.

### Koyeb Deployment Setup

1. **Attach a Persistent Volume:**
   - In your Koyeb App settings under **Volumes**, add a persistent volume (e.g. `openproxy-data` mounted at `/data`).
2. **Configure Database Path:**
   - In your `config.toml` (or via environment variables), set:
     ```toml
     [storage]
     database_path = "/data/openproxy.db"
     ```
3. **Set the Master Encryption Key:**
   - Define `OPENPROXY_MASTER_KEY` in your Koyeb environment variables (base64 32-byte key) so credentials remain decryptable across redeploys:
     ```bash
     openssl rand -base64 32
     ```
4. **Before Upgrading the Image:**
   - Always download an `openproxy-backup.json` from the Dashboard or API.
   - If an unexpected volume wipe occurs, simply upload `openproxy-backup.json` to immediately restore all providers, accounts, and combos.

### Ephemeral / Distroless Deployments (Without Persistent Volumes)

If running on an ephemeral container platform without attached volumes where the 0600 file (`bootstrap-api-key.txt`) cannot be retrieved via shell or platform console, you can explicitly opt in to logging the bootstrap key:

- Set `OPENPROXY_LOG_BOOTSTRAP_KEY=1` (or `true`) in your environment variables.
- When enabled, OpenProxy logs a high-visibility security warning and prints the bootstrap API key once to standard server logs upon initial generation.
- **Security Note:** Anyone with access to platform logs will be able to read the bootstrap administrative key. Disable this variable once the initial key has been saved.

### Docker Run with Persistent Volume

```bash
# 1. Create a named volume
docker volume create openproxy-data

# 2. Run OpenProxy with persistent storage and master key
docker run -d \
  --name openproxy \
  -p 8787:8787 \
  -v openproxy-data:/data \
  -e OPENPROXY_CONFIG=/data/config.toml \
  -e OPENPROXY_MASTER_KEY="<YOUR_BASE64_KEY>" \
  ghcr.io/soyelmismo/openproxy:latest
```

---

## 5. Security Model & Architecture

- **Portability Across Environments:** Account secrets in SQLite are sealed with `OPENPROXY_MASTER_KEY`. During export, secrets are decrypted with the source host's master key and placed in the bundle (optionally encrypted with a user passphrase). When restored onto a new host, secrets are re-encrypted using the new host's `OPENPROXY_MASTER_KEY`.
- **Atomic Transaction & Safety Backup:** Restores run inside an immediate SQLite transaction with foreign keys verified before commit. An automatic snapshot of the pre-existing SQLite database is saved prior to modifying any tables.
