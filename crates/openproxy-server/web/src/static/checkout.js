(function () {
  var order = null, pollTimer = null;
  var packagesCache = [];

  function tab(name) {
    ["step-buy", "step-mykey"].forEach(function (id) { document.getElementById(id).classList.add("hidden"); });
    document.getElementById(name).classList.remove("hidden");
  }
  document.getElementById("tab-buy").addEventListener("click", function () { tab("step-buy"); });
  document.getElementById("tab-mykey").addEventListener("click", function () { tab("step-mykey"); });

  // ── my key: balance + history + top-up with the key itself ──
  document.getElementById("mykey-load").addEventListener("click", function () {
    var key = document.getElementById("mykey").value.trim();
    var errEl = document.getElementById("mykey-err");
    errEl.classList.add("hidden");
    if (!key) return;
    fetch("v1/credits/me/balance", { headers: { "Authorization": "Bearer " + key } })
      .then(function (r) { if (!r.ok) throw new Error("invalid key or credits disabled"); return r.json(); })
      .then(function (b) {
        document.getElementById("mykey-data").classList.remove("hidden");
        document.getElementById("my-balance").textContent = "USDC " + b.balance_usd;
        var lots = (b.lots || []).map(function (l) { return "USDC " + l.remaining_usd + " — expires " + (l.expires_at || "never"); });
        document.getElementById("my-expiry").textContent = lots.join(" | ") || "No active lots.";
        return fetch("v1/credits/me/transactions?limit=50", { headers: { "Authorization": "Bearer " + key } });
      })
      .then(function (r) { return r.json(); })
      .then(function (txs) {
        var el = document.getElementById("my-history");
        el.innerHTML = "";
        (txs || []).slice(0, 20).forEach(function (t) {
          var row = document.createElement("div");
          row.style.marginBottom = "4px";
          var link = t.basescan_url ? '<a style="color:var(--color-primary)" href="' + t.basescan_url + '" target="_blank" rel="noopener">view on BaseScan</a>' : "—";
          row.innerHTML = t.created_at + " · " + t.kind + " · " + (t.kind === "topup" ? "+" : "") + t.amount_usd + " USDC · " + t.status + " · " + link;
          el.appendChild(row);
        });
        if (!txs || txs.length === 0) el.textContent = "No transactions yet.";
      })
      .catch(function (e) { errEl.textContent = e.message; errEl.classList.remove("hidden"); });
  });

  document.getElementById("my-topup").addEventListener("click", function () {
    var key = document.getElementById("mykey").value.trim();
    var errEl = document.getElementById("mykey-err");
    errEl.classList.add("hidden");
    fetch("v1/credits/me/topup", {
      method: "POST",
      headers: { "Content-Type": "application/json", "Authorization": "Bearer " + key },
      body: JSON.stringify({ usd: parseFloat(document.getElementById("my-usd").value || 0) }),
    })
      .then(function (r) { return r.json().then(function (b) { return { ok: r.ok, body: b }; }); })
      .then(function (res) {
        if (!res.ok) throw new Error((res.body && res.body.error && res.body.error.message) || "top-up failed");
        order = res.body;
        hide("step-mykey"); show("step-pay");
        document.getElementById("qr").innerHTML = order.qr_svg;
        document.getElementById("amount").textContent = order.amount_display + " USDC";
        document.getElementById("addr").textContent = order.collector_address;
        startCountdown(); startPolling();
      })
      .catch(function (e) { errEl.textContent = e.message; errEl.classList.remove("hidden"); });
  });

  function show(id) { document.getElementById(id).classList.remove("hidden"); }
  function hide(id) { document.getElementById(id).classList.add("hidden"); }
  function err(id, msg) {
    var e = document.getElementById(id);
    e.textContent = msg; e.classList.remove("hidden");
  }

  fetch("v1/credits/packages", { headers: { "Accept": "application/json" } })
    .then(function (r) { return r.json(); })
    .then(function (p) {
      if (!p.enabled) {
        var closed = p.usdc_address && p.collector_address;
        throw new Error(closed
          ? "The payment gateway is temporarily closed. Please try again later."
          : "on-chain top-ups are not enabled on this server");
      }
      packagesCache = p.packages || [];
      var hasPresets = packagesCache.some(function (pk) { return pk.package_id != null; });
      if (hasPresets) {
        // Duration is priced through the presets (longer hold = better
        // per-day rate). Free amounts ride the shortest lifetime so nobody
        // buys a 30-day balance at the 7-day price outside the presets.
        var ttlSel = document.getElementById("ttl");
        var ttlWrap = ttlSel ? ttlSel.parentElement : null;
        if (ttlWrap) ttlWrap.style.display = "none";
      }
      var usd = document.getElementById("usd");
      packagesCache.forEach(function (pk) {
        var o = document.createElement("option");
        if (pk.package_id != null) {
          o.value = "pkg:" + pk.package_id;
          o.textContent = pk.label + " — $" + pk.usd + " / " + pk.ttl_days + " days";
        } else {
          o.value = pk.usd; o.textContent = pk.label + " — $" + pk.usd;
        }
        usd.appendChild(o);
      });
      var ttl = document.getElementById("ttl");
      p.ttl_days.forEach(function (d) {
        var o = document.createElement("option");
        o.value = d; o.textContent = d + " days";
        ttl.appendChild(o);
      });
      ttl.value = "15";
    })
    .catch(function (e) { err("page-err", "Could not load packages: " + e.message); hide("step-buy"); });

  document.getElementById("create").addEventListener("click", function () {
    var btn = this; btn.disabled = true;
    fetch("v1/credits/order", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: (function () {
        var v = String(document.getElementById("usd").value);
        if (v.indexOf("pkg:") === 0) {
          return JSON.stringify({ package_id: parseInt(v.slice(4), 10), mint_new_key: true });
        }
        return JSON.stringify({
          usd: parseFloat(v),
          ttl_days: parseInt(document.getElementById("ttl").value, 10),
          mint_new_key: true,
        });
      })(),
    })
      .then(function (r) { return r.json().then(function (b) { return { ok: r.ok, body: b }; }); })
      .then(function (res) {
        if (!res.ok) throw new Error((res.body && res.body.error && res.body.error.message) || "order failed");
        order = res.body;
        hide("step-buy"); show("step-pay");
        document.getElementById("qr").innerHTML = order.qr_svg;
        document.getElementById("amount").textContent = order.amount_display + " USDC";
        document.getElementById("addr").textContent = order.collector_address;
        startCountdown();
        startPolling();
      })
      .catch(function (e) { btn.disabled = false; err("buy-err", e.message); });
  });

  function startCountdown() {
    var deadline = new Date(order.expires_at).getTime();
    function tick() {
      var left = Math.max(0, Math.floor((deadline - Date.now()) / 1000));
      var m = Math.floor(left / 60), s = left % 60;
      document.getElementById("timer").textContent =
        left > 0 ? "Price holds for " + m + ":" + String(s).padStart(2, "0") : "Expired";
      if (left > 0) setTimeout(tick, 1000);
      else { stopPolling(); document.getElementById("pay-status").textContent = "Order expired. Create a new one."; }
    }
    tick();
  }

  function startPolling() {
    pollTimer = setInterval(function () {
      fetch("v1/credits/order/" + order.order_code, { headers: { "Accept": "application/json" } })
        .then(function (r) { return r.json(); })
        .then(function (st) {
          if (st.status === "paid" || st.status === "minted") {
            stopPolling();
            document.getElementById("pay-status").textContent = "Paid. Confirmed on-chain (block " + st.block_number + ").";
            hide("step-pay"); show("step-key");
          }
        })
        .catch(function () { /* transient: keep polling */ });
    }, 5000);
  }
  function stopPolling() { if (pollTimer) { clearInterval(pollTimer); pollTimer = null; } }

  document.getElementById("reveal").addEventListener("click", function () {
    var btn = this;
    btn.disabled = true;
    fetch("v1/credits/order/" + order.order_code + "/reveal", { method: "POST" })
      .then(function (r) { return r.json().then(function (b) { return { ok: r.ok, body: b }; }); })
      .then(function (res) {
        if (!res.ok) throw new Error((res.body && res.body.error && res.body.error.message) || "reveal failed");
        var el = document.getElementById("key");
        el.textContent = res.body.key;
        el.classList.remove("hidden");
        btn.classList.add("hidden");
      })
      .catch(function (e) { btn.disabled = false; err("page-err", e.message); });
  });
})();
