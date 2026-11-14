#!/usr/bin/env python3
"""Regtest probe of the prioritisetransaction side effects described in
docs/design/template-policy.md, "What the delta changes besides block
building": replacement, eviction (and compact-block reconstruction after it),
and refusal on return.

Three nodes, connected peer <-> pool <-> relay:
  pool  : the operator's node. Sets the deltas, -maxmempool=5.
  peer  : no deltas, default mempool. Mines, and stands in for the network.
  relay : carries the same delta as pool and forwards a replacement to it
          over P2P, the way a replacement normally arrives.

usage: template-policy-delta-probe.py <bitcoind> <workdir> <big|zero> [p2p_base]
  big  : fee delta -1,000,000,003 sats (the design's delta)
  zero : fee delta = -(real fee), so the modified fee is exactly 0

The workdir is deleted and recreated. Ports p2p_base to p2p_base + 21 are
used (default 18700). Works with Bitcoin Core and Bitcoin Knots; on Knots it
also sends the -1e17 priority delta and sets -blockprioritysize=0, as the pool
would. Results go to <workdir>/results.json. Standard library only.
"""
import base64, json, os, re, shutil, subprocess, sys, time, urllib.error, urllib.request
from decimal import Decimal

BIG = -1_000_000_003
KNOTS_PRIO = -1e17


class RPCError(Exception):
    pass


def btc(sat):
    return f"{sat // 10**8}.{sat % 10**8:08d}"


def sat(amount):
    return int(Decimal(str(amount)) * 10**8)


class Node:
    def __init__(self, name, bitcoind, root, p2p, rpc, extra):
        self.name, self.bitcoind, self.p2p, self.rpcport = name, bitcoind, p2p, rpc
        self.dir = os.path.join(root, name)
        self.extra = extra
        self.proc = None

    def start(self):
        os.makedirs(self.dir, exist_ok=True)
        args = [self.bitcoind, "-regtest", f"-datadir={self.dir}", f"-port={self.p2p}",
                f"-bind=127.0.0.1:{self.p2p}", "-listenonion=0", "-dnsseed=0",
                "-fixedseeds=0", f"-rpcport={self.rpcport}", "-rpcbind=127.0.0.1",
                "-rpcallowip=127.0.0.1", "-server=1", "-fallbackfee=0.0001",
                "-printtoconsole=0", "-logtimemicros=1", "-debug=cmpctblock",
                "-debug=mempool", "-debug=mempoolrej", "-rpcthreads=8"] + self.extra
        self.proc = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(300):
            try:
                self.rpc("getblockchaininfo")
                return
            except Exception:
                time.sleep(0.2)
        raise RuntimeError(f"{self.name} did not start")

    def stop(self):
        if self.proc and self.proc.poll() is None:
            try:
                self.rpc("stop")
            except Exception:
                pass
            try:
                self.proc.wait(60)
            except subprocess.TimeoutExpired:
                self.proc.kill()

    def rpc(self, method, *params, wallet=None):
        cookie = open(os.path.join(self.dir, "regtest", ".cookie")).read().strip()
        url = f"http://127.0.0.1:{self.rpcport}/" + (f"wallet/{wallet}" if wallet else "")
        body = json.dumps({"jsonrpc": "1.0", "id": 1, "method": method, "params": list(params)},
                          default=str).encode()
        req = urllib.request.Request(url, data=body, headers={
            "Authorization": "Basic " + base64.b64encode(cookie.encode()).decode(),
            "Content-Type": "application/json"})
        try:
            resp = urllib.request.urlopen(req, timeout=600)
        except urllib.error.HTTPError as e:
            resp = e
        out = json.loads(resp.read(), parse_float=Decimal)
        if out.get("error"):
            raise RPCError(out["error"])
        return out["result"]

    def log(self):
        return open(os.path.join(self.dir, "regtest", "debug.log")).read()


def log(msg):
    print(msg, flush=True)


def main():
    bitcoind, work, mode = sys.argv[1], sys.argv[2], sys.argv[3]
    base = int(sys.argv[4]) if len(sys.argv) > 4 else 18700
    assert mode in ("big", "zero")
    version = subprocess.run([bitcoind, "-version"], capture_output=True, text=True).stdout.splitlines()[0]
    knots = "Knots" in version
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    node_extra = ["-blockprioritysize=0"] if knots else []
    pool = Node("pool", bitcoind, work, base, base + 1, ["-maxmempool=5"] + node_extra)
    peer = Node("peer", bitcoind, work, base + 10, base + 11, node_extra)
    relay = Node("relay", bitcoind, work, base + 20, base + 21, node_extra)
    results = {"node": version, "mode": mode}
    try:
        run(pool, peer, relay, knots, mode, results)
    finally:
        pool.stop()
        peer.stop()
        relay.stop()
        with open(os.path.join(work, "results.json"), "w") as f:
            json.dump(results, f, indent=2, default=str)
        log(json.dumps(results, indent=2, default=str))


def run(pool, peer, relay, knots, mode, R):
    # Topology: peer <-> pool <-> relay. The peer and relay are not connected.
    pool.start()
    peer.start()
    relay.start()
    pool.rpc("addnode", f"127.0.0.1:{peer.p2p}", "onetry")
    pool.rpc("addnode", f"127.0.0.1:{relay.p2p}", "onetry")
    wait(lambda: len(pool.rpc("getpeerinfo")) >= 2 and len(peer.rpc("getpeerinfo")) >= 1
         and len(relay.rpc("getpeerinfo")) >= 1, "peering")
    R["mempoolinfo_pool"] = {k: pool.rpc("getmempoolinfo")[k] for k in
                             ("maxmempool", "minrelaytxfee", "incrementalrelayfee")}

    W = "w"
    peer.rpc("createwallet", W)
    mine(peer, pool, 110, W)

    def newaddr():
        return peer.rpc("getnewaddress", "", "bech32", wallet=W)

    # Fan out one mature coinbase into many 0.3 BTC coins.
    cb = [u for u in peer.rpc("listunspent", 100, wallet=W) if sat(u["amount"]) == 50 * 10**8][0]
    outs = {newaddr(): btc(30_000_000) for _ in range(150)}
    outs[newaddr()] = btc(50 * 10**8 - 150 * 30_000_000 - 100_000)
    fan = sign(peer, W, [(cb["txid"], cb["vout"])], outs)
    peer.rpc("sendrawtransaction", fan)
    mine(peer, pool, 1, W)
    coins = [u for u in peer.rpc("listunspent", 1, wallet=W) if sat(u["amount"]) == 30_000_000]
    log(f"{len(coins)} coins ready")

    def delta_for(fee):
        return BIG if mode == "big" else -fee

    def prioritise(node, txid, fee_delta, sign_=1):
        if knots:
            node.rpc("prioritisetransaction", txid, KNOTS_PRIO * sign_, fee_delta)
        else:
            node.rpc("prioritisetransaction", txid, 0, fee_delta)

    def deltas(node):
        return node.rpc("getprioritisedtransactions")

    # ---------------- 1. Replacement ----------------
    c = coins.pop()
    a_fee, b_fee = 10_000, 300
    A = sign(peer, W, [(c["txid"], c["vout"])], {newaddr(): btc(30_000_000 - a_fee)})
    B = sign(peer, W, [(c["txid"], c["vout"])], {newaddr(): btc(30_000_000 - b_fee)})
    a_txid = peer.rpc("decoderawtransaction", A)["txid"]
    b_info = peer.rpc("decoderawtransaction", B)
    send_both(pool, peer, A)
    r1 = {"A_fee_sat": a_fee, "B_fee_sat": b_fee, "B_vsize": b_info["vsize"]}
    r1["pool_B_before_delta"] = tma(pool, B)
    prioritise(pool, a_txid, delta_for(a_fee))
    r1["delta_on_A"] = deltas(pool).get(a_txid)
    r1["pool_B_after_delta"] = tma(pool, B)
    r1["peer_B"] = tma(peer, B)
    try:
        pool.rpc("sendrawtransaction", B)
        r1["pool_send_B"] = "accepted"
    except RPCError as e:
        r1["pool_send_B"] = str(e)
    r1["delta_on_A_after_replacement"] = deltas(pool).get(a_txid, "cleared")
    time.sleep(20)
    r1["after_20s"] = {
        "pool_has_A": a_txid in pool.rpc("getrawmempool"),
        "pool_has_B": b_info["txid"] in pool.rpc("getrawmempool"),
        "peer_has_A": a_txid in peer.rpc("getrawmempool"),
        "peer_has_B": b_info["txid"] in peer.rpc("getrawmempool"),
    }
    # The peer mines A, which the pool replaced: does the pool need a round trip?
    blk = mine(peer, pool, 1, W)[0]
    r1["peer_mines_A"] = reconstruct(pool, blk)
    r1["delta_on_A_after_A_confirmed"] = deltas(pool).get(a_txid, "cleared")
    r1["pool_rejected_A_from_a_peer"] = rejected_from_peer(pool, a_txid)
    R["1_replacement"] = r1
    log(f"replacement: {json.dumps(r1, default=str)}")

    # ---------------- 1b. Replacement arriving by P2P ----------------
    # The relay node carries the same delta, accepts B and relays it to the pool
    # node, which is how a replacement normally reaches the operator's node.
    c = coins.pop()
    A = sign(peer, W, [(c["txid"], c["vout"])], {newaddr(): btc(30_000_000 - a_fee)})
    B = sign(peer, W, [(c["txid"], c["vout"])], {newaddr(): btc(30_000_000 - b_fee)})
    a_txid = peer.rpc("decoderawtransaction", A)["txid"]
    b_txid = peer.rpc("decoderawtransaction", B)["txid"]
    for n in (peer, pool, relay):
        n.rpc("sendrawtransaction", A)
    prioritise(pool, a_txid, delta_for(a_fee))
    prioritise(relay, a_txid, delta_for(a_fee))
    relay.rpc("sendrawtransaction", B)
    t0 = time.time()
    wait(lambda: b_txid in pool.rpc("getrawmempool"), "B relayed to pool", 120)
    r1b = {"B_reached_pool_after_s": round(time.time() - t0, 1),
           "pool_accepted_B_from_peer": bool(re.search(
               r"AcceptToMemoryPool: peer=\d+: accepted " + b_txid, pool.log())),
           "delta_on_A_after_replacement": deltas(pool).get(a_txid, "cleared"),
           "peer_has_A": a_txid in peer.rpc("getrawmempool")}
    blk = mine(peer, pool, 1, W)[0]
    r1b["peer_mines_A"] = reconstruct(pool, blk)
    r1b["pool_rejected_A_from_a_peer"] = rejected_from_peer(pool, a_txid)
    R["1b_replacement_p2p"] = r1b
    log(f"replacement p2p: {json.dumps(r1b, default=str)}")

    # ---------------- 2. Eviction, 3. refusal, compact reconstruction ----------------
    c = coins.pop()
    u_fee = 50 * 141  # ~50 sat/vB at ~141 vB (1-in, 2-out P2WPKH)
    u_change = newaddr()
    U = sign(peer, W, [(c["txid"], c["vout"])],
             {newaddr(): btc(10_000_000), u_change: btc(20_000_000 - u_fee)})
    u = peer.rpc("decoderawtransaction", U)
    u_vout = [o["n"] for o in u["vout"] if o["scriptPubKey"].get("address") == u_change][0]
    send_both(pool, peer, U)
    c_fee = 100 * 110
    C = sign(peer, W, [(u["txid"], u_vout)], {newaddr(): btc(20_000_000 - u_fee - c_fee)})
    cx = peer.rpc("decoderawtransaction", C)
    send_both(pool, peer, C)
    # L: a lone unwanted transaction, same shape and feerate as U, no child.
    c = coins.pop()
    L = sign(peer, W, [(c["txid"], c["vout"])],
             {newaddr(): btc(10_000_000), newaddr(): btc(20_000_000 - u_fee)})
    lx = peer.rpc("decoderawtransaction", L)
    send_both(pool, peer, L)
    prioritise(pool, u["txid"], delta_for(u_fee))
    prioritise(pool, lx["txid"], delta_for(u_fee))
    r2 = {"U_feerate": round(u_fee / u["vsize"], 1), "C_feerate": round(c_fee / cx["vsize"], 1),
          "delta_on_U": deltas(pool).get(u["txid"]), "delta_on_L": deltas(pool).get(lx["txid"])}
    time.sleep(6)  # past the GBT cache window
    gbt = {t["txid"] for t in pool.rpc("getblocktemplate", {"rules": ["segwit"]})["transactions"]}
    r2["pool_template_has"] = {"U": u["txid"] in gbt, "C": cx["txid"] in gbt, "L": lx["txid"] in gbt}

    # Fill with 5 sat/vB transactions until the pool node evicts U.
    desc = [d["desc"] for d in peer.rpc("listdescriptors", wallet=W)["descriptors"]
            if d["desc"].startswith("wpkh(") and not d.get("internal")][0]
    fill_addrs = peer.rpc("deriveaddresses", desc, [2000, 2000 + 2000 * 60])
    fill_rate = 5
    fills, refused_fill = [], 0
    evicted_at = l_evicted_at = None
    for i in range(60):
        c = coins.pop()
        payouts = {a: btc(5000) for a in fill_addrs[i * 2000:(i + 1) * 2000]}
        tx = build_at_rate(peer, W, c, payouts, newaddr(), fill_rate)
        fills.append(peer.rpc("decoderawtransaction", tx)["txid"])
        if not send_both(pool, peer, tx):
            refused_fill += 1
        now = pool.rpc("getrawmempool")
        if lx["txid"] not in now and l_evicted_at is None:
            l_evicted_at = i + 1
        if u["txid"] not in now and evicted_at is None:
            evicted_at = i + 1
        if evicted_at and l_evicted_at:
            break
    mp = pool.rpc("getmempoolinfo")
    pool_mp, peer_mp = set(pool.rpc("getrawmempool")), set(peer.rpc("getrawmempool"))
    r2.update({
        "fill_feerate": fill_rate,
        "fills_sent_until_U_evicted": evicted_at,
        "fills_sent_until_L_evicted": l_evicted_at,
        "L_in_pool": lx["txid"] in pool_mp,
        "delta_on_L_after_fill": deltas(pool).get(lx["txid"], "cleared"),
        "fills_refused_by_pool": refused_fill,
        "pool_mempool": {"size": mp["size"], "usage": mp["usage"], "mempoolminfee": mp["mempoolminfee"]},
        "U_in_pool": u["txid"] in pool_mp, "C_in_pool": cx["txid"] in pool_mp,
        "fills_missing_from_pool": len([t for t in fills if t not in pool_mp]),
        "pool_missing_vs_peer": len(peer_mp - pool_mp),
        "delta_on_U_after_eviction": deltas(pool).get(u["txid"], "cleared"),
    })
    log(f"eviction: {json.dumps(r2, default=str)}")

    # 3. Refusal on return: U rebroadcast to the pool node while the delta stands.
    r3 = {}
    for name, hx, tid in (("U", U, u["txid"]), ("L", L, lx["txid"])):
        r3[f"{name}_with_delta"] = tma(pool, hx)
        prioritise(pool, tid, -delta_for(u_fee), sign_=-1)
        r3[f"{name}_delta_after_cancel"] = deltas(pool).get(tid, "cleared")
        r3[f"{name}_delta_cancelled"] = tma(pool, hx)
        prioritise(pool, tid, delta_for(u_fee))
        r3[f"{name}_delta_reapplied"] = tma(pool, hx)
    R["3_refusal_on_return"] = r3
    log(f"refusal: {json.dumps(r3, default=str)}")

    # Compact-block reconstruction when the peer mines U and C.
    missing_before = peer_mp - pool_mp
    blk = mine(peer, pool, 1, W)[0]
    block_txids = set(peer.rpc("getblock", blk)["tx"][1:])
    r2["peer_block"] = {
        "txs": len(block_txids),
        "has_U": u["txid"] in block_txids, "has_C": cx["txid"] in block_txids,
        "has_L": lx["txid"] in block_txids,
        "block_txs_pool_lacked": len(block_txids & missing_before),
        "reconstruct": reconstruct(pool, blk),
    }
    # Control: mine the rest, then a block of three fresh small txs both nodes have.
    for _ in range(6):
        if not peer.rpc("getrawmempool"):
            break
        mine(peer, pool, 1, W)
    for _ in range(3):
        c = coins.pop()
        send_both(pool, peer, sign(peer, W, [(c["txid"], c["vout"])], {newaddr(): btc(30_000_000 - 2000)}))
    blk = mine(peer, pool, 1, W)[0]
    r2["control_block"] = reconstruct(pool, blk)
    R["2_eviction"] = r2
    log(f"eviction+blocks: {json.dumps(r2, default=str)}")


def sign(node, W, inputs, outs):
    ins = [{"txid": t, "vout": v, "sequence": 0xFFFFFFFD} for t, v in inputs]
    raw = node.rpc("createrawtransaction", ins, outs)
    s = node.rpc("signrawtransactionwithwallet", raw, wallet=W)
    assert s["complete"], s
    return s["hex"]


def build_at_rate(node, W, coin, payouts, change, rate):
    total = sat(coin["amount"])
    paid = sum(sat(v) for v in payouts.values())
    fee = 0
    for _ in range(3):
        outs = dict(payouts)
        outs[change] = btc(total - paid - fee)
        tx = sign(node, W, [(coin["txid"], coin["vout"])], outs)
        vsize = node.rpc("decoderawtransaction", tx)["vsize"]
        if fee >= rate * vsize:
            return tx
        fee = rate * vsize + 2
    return tx


def send_both(pool, peer, tx):
    ok = True
    for n in (peer, pool):
        try:
            n.rpc("sendrawtransaction", tx)
        except RPCError as e:
            if "already" in str(e):
                continue
            if n is pool:
                ok = False
            else:
                raise
    return ok


def tma(node, tx):
    r = node.rpc("testmempoolaccept", [tx])[0]
    return {"allowed": r["allowed"], "reason": r.get("reject-reason"), "details": r.get("reject-details")}


def mine(peer, pool, n, W):
    hashes = peer.rpc("generatetoaddress", n, peer.rpc("getnewaddress", wallet=W))
    wait(lambda: pool.rpc("getbestblockhash") == hashes[-1], "block sync", 120)
    return hashes


def reconstruct(pool, blk):
    text = pool.log()
    out = {}
    m = re.search(r"^(\S+) .*Initialized PartiallyDownloadedBlock for block " + blk + r".*$", text, re.M)
    if m:
        out["init_line"] = m.group(0)[-160:]
    m2 = re.search(r"^(\S+) .*Successfully reconstructed block " + blk + r".*$", text, re.M)
    if m2:
        out["line"] = m2.group(0)
        if m:
            out["init_to_reconstructed_ms"] = round(
                (ts(m2.group(1)) - ts(m.group(1))) * 1000, 2)
    if not out:
        out["note"] = "no compact-block lines for this block (full block delivery?)"
    return out


def rejected_from_peer(node, txid):
    """True if the node logged a P2P rejection of txid. A rejected tx also goes
    into the compact-block extra pool, which would explain an extra-pool hit."""
    return bool(re.search(txid + r" \(wtxid=\w+\) from peer=\d+ was not accepted", node.log()))


def ts(s):
    from datetime import datetime
    return datetime.strptime(s.rstrip("Z"), "%Y-%m-%dT%H:%M:%S.%f").timestamp()


def wait(fn, what, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        try:
            if fn():
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise RuntimeError(f"timeout waiting for {what}")


if __name__ == "__main__":
    main()
