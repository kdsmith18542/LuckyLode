# Security Policy

## Reporting a vulnerability

Please report vulnerabilities **privately** using GitHub's private
vulnerability reporting: **Security → Report a vulnerability** on this
repository, or on [LuckyLode-node](https://github.com/kdsmith18542/LuckyLode-node)
for node issues. Please do not open public issues for exploitable
problems.

Include what you found, the affected component (node, pool, miner,
wallet, key derivation), reproduction steps, and impact. A proof of
concept helps but is not required.

## Scope

In scope: the consensus node (LuckyLode-node), the reference pool and
stratum implementation, the miner, the wallet core and desktop wallet,
and the key derivation and transaction signing paths.

Out of scope: social engineering, attacks against deployed
infrastructure outside this software, and findings in upstream
Verus/Komodo/Zcash code that are not reachable from Lucky Lode's
configuration (report those upstream — the node repository retains
their `responsible_disclosure.md`).

## Handling

Reports are triaged as they arrive. Consensus-relevant findings are
taken most seriously and handled before disclosure; fixes land with
tests, and coordinated disclosure happens after a release window.
There is no bug bounty program at this stage; credit in the release
notes is always given.
