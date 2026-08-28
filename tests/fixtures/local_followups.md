## Backend headers never drive commitment CPFP retries

The production `ChainWatcher` block callback advances monitors and emits `block`, but the node's listener never calls `reCpfpStuckCommitments`. A scratch wiring test delivered a subscribed backend header and observed zero calls. The manual `LightningNode.handleNewBlock` control made one call.

## Problem

Configured chain backends route accepted headers through `ChainWatcher.handleNewBlock` and the node's `block` listener. Neither invokes `ChannelManager.reCpfpStuckCommitments`; only the public manual block entry point does.

## Reproduction

1. Configure a `LightningNode` with a `ChainWatcher` backend and replace `reCpfpStuckCommitments` with a call counter.
2. Start the watcher and deliver height 101 through the subscribed header callback.
3. Observe zero calls.
4. Call `node.handleNewBlock(102)` and observe one call.

## Impact

Normal configured-backend nodes do not retry stuck commitment packages or remove superseded entries on new blocks. Users can lose time-sensitive HTLC claims when a low-fee force-close package remains pinned.

## Expected behavior

1. Run `reCpfpStuckCommitments` exactly once for each accepted backend header using the live force-close feerate.
2. Preserve the manual block path without running the pass twice for one header.
3. Cover the subscribed backend callback in a regression test.
4. This behavior predates this branch.

Found while working on #589.

## Overlapping checkOutputSpend scans can move a recorded spend height backward

## Problem

`checkOutputSpend` (src/lightning/chain/chain-watcher.ts) applies its result with no arbitration against a scan that started later. Its only guards are the lifecycle generation and the `watchedOutputs.get(key) !== watched` map-identity check, and both still pass for two concurrent scans of the same live watch.

Two scans of one output run concurrently in normal operation: `handleNewBlock` fires one per watched output, and the scripthash subscription callback fires another. Either can stall in `getScriptHashHistory` or `getTransaction` while the other completes.

Before #585 this was harmless: the apply condition was `watched.spendTxid !== spend.txid || watched.spendUnverified`, so an older scan re-observing the same spend was a no-op. #585 added `watched.spendHeight !== spend.height` (needed to recount depth after a re-mine), and that arm also admits a move *backward*.

The funding-spend side already has exactly this arbitration (`channelSpendScans`, `beginSpendScan` / `outpointScanOvertaken` / `recordScanCompleted`), including the comment explaining that freshness is start order, not completion order. The output-spend side has none.

## Reproduction

1. Watch an output whose spend is at height 100; set the watcher tip to 190.
2. Start scan A. Its history says height 100; stall it inside `getTransaction`.
3. The spend is re-mined at 190. Start scan B, which completes: `watched.spendHeight` becomes 190 and `handleOutputSpent(..., 190)` reaches the manager.
4. Release scan A. It resumes holding its stale history.

Actual result:

- `handleOutputSpent` is called twice, with heights `[190, 100]`.
- `watched.spendHeight` ends at 100, 90 blocks below where the spend actually sits.
- The monitor's tracked output carries confirmationHeight 100, so its finality clock counts from there.
- At tip 200 the retention check (`currentBlockHeight - spend.height + 1 >= SPEND_FINALITY_DEPTH`, 100) computes 101 off the stale height and deletes the watch, so nothing looks again.

## Impact

This is the #576 fund-loss shape reopened from the other end. A sweep or penalty that was reorged out and re-mined shallowly gets counted as ~90+ blocks deep, can be promoted to irrevocably resolved, and the watch that would have corrected it is retired. A breach penalty reorged out after that point goes unpunished, because nothing rebroadcasts it.

## Expected behavior

- Per-output scan arbitration on `checkOutputSpend`, mirroring `channelSpendScans`: take a ticket before the first await, and retire a scan whose ticket is older than one that already completed or applied for the same output key.
- Arbitrate on scan START order, not completion order: the later-started scan holds the fresher history whichever finishes first.
- The absence branch (`handleOutputUnspent`) needs the same protection: an older scan must not retract a spend a newer scan just observed.
- Regression test: two concurrent scans of one output where the older one stalls and resumes after the newer completes, asserting the manager sees only the newer height and the watch retains it.

Found while working on #589.

## Force close accepts a CLOSED channel whose sub-relay-fee coop close already confirmed

## Problem

`closedWithUnbroadcastableCoopClose` (src/lightning/channel/channel.ts) is the escape hatch that lets `prepareForceClose` rescue a CLOSED channel stranded on a mutual close no mempool will relay. Its relay-floor arm judges the recorded tx alone and never asks whether that tx is already on chain.

The other arms are self-limiting: a future or timestamp-space locktime cannot describe a confirmed transaction. A fee under the relay floor can, since a miner can include such a tx out of band, and #579 is precisely about our negotiation having signed those.

`ChannelManager.forceClose` then unconditionally constructs a new `ChainMonitor` and does `this.monitors.set(idHex, monitor)`, so the rescue discards whatever the old monitor knew. Note `rebroadcastClose` already gates on `monitor.isCommitmentConfirmed()` for the same reason.

## Reproduction

1. Open a 1,000,000 sat channel.
2. Record a mutual close spending the funding outpoint with a single 999,999 sat output (1 sat fee, under `minRelayFeeForWeight`) as `lastCooperativeCloseTxHex`, and set the channel to CLOSED.
3. Advance the tip to 800000 and report that close as the funding spend, confirmed at 800000. The monitor reads `isCommitmentConfirmed() === true` with `commitmentBroadcast.txid` equal to the close.
4. Call `forceCloseChannel(channelId, destScript)`.

Actual result:

- Returns `{ ok: true, commitmentTxid: ... }` and broadcasts a commitment spending an already-spent funding output.
- The channel moves from CLOSED to FORCE_CLOSED.
- The confirmed monitor is replaced: the new one reports `isCommitmentConfirmed() === false` and `commitmentBroadcast === undefined`.

## Impact

The confirmed close's record is destroyed. Its tracked outputs, its classification and its irrevocable-depth clock are gone, and the replacement monitor waits for a commitment that can never confirm, so the channel never resolves and the watch is never retired. The channel is also mislabelled FORCE_CLOSED with a close reason that misreports what happened on chain, and `closeStatus` reports the commitment txid rather than the close that actually settled. An automatic stuck-channel escalation reaching `prepareForceClose` triggers this without any operator action.

## Expected behavior

- `prepareForceClose` refuses the CLOSED recovery when the recorded funding spend is already confirmed, before any commitment is built, and returns a distinguishable error rather than plain `wrong state`.
- The check belongs where confirmation is actually known. `closedWithUnbroadcastableCoopClose` is on `Channel`, which has no monitor, so either the manager gates the CLOSED arm before calling `prepareForceClose` or the channel is given the confirmation fact to judge against.
- `ChannelManager.forceClose` should not replace a monitor that has a confirmed funding spend recorded, whatever admitted the plan.
- Regression test: a confirmed 1-sat-fee mutual close, then `forceClose`, asserting the refusal, that the channel stays CLOSED, and that the monitor still reports the close confirmed.

Found while working on #589.

## Forwarded fail refunds upstream before the downstream removal round completes

A forwarded inbound leg is failed upstream the moment the peer's update_fail_htlc arrives, while the downstream removal round is still provisional, so a disconnect-and-refulfill leaves us paying downstream for a forward we already refunded upstream. This is the #590 defect class through the primary propagation path, which this PR did not touch: `handleHtlcFailed` (src/lightning/node/lightning-node.ts:16272) sees the forward linkage and calls `failForwardUpstream` synchronously from the `htlc:failed` event, and `failForwardUpstream` fails the inbound leg and deletes the linkage with no check on the removal phase flags. I confirmed it with a scratch mocha test on a three-node fixture (Bob-Alice-Carol) driving the real handlers: `handleUpdateFailHtlc` + `processActions` left the outbound entry FAILED with `removalLocallyRevoked=false, removalRemoteCommitted=false` while the inbound was already FAILED and the linkage gone; `handlePeerDisconnected` rolled the outbound leg back to COMMITTED; a retransmitted `handleUpdateFulfillHtlc` was accepted (HTLC_FULFILLED, preimage learned); final `canFulfillHtlc(7n)` was false and `settleForwardsOwedUpstream` did nothing. A malicious payee controls the window by withholding its commitment_signed, so no timing luck is needed. Predates this branch: master's `handleHtlcFailed` has the same immediate `failForwardUpstream` call.

## Problem

`handleHtlcFailed` treats the peer's `update_fail_htlc` as terminal on arrival. For a forwarded HTLC it calls `failForwardUpstream`, which fails the inbound leg via `channelManager.failHtlc` and consumes the `forwardedHtlcs` linkage, while the outbound entry is FAILED with both removal flags false. The removal round on the outbound channel has not run: `markForReestablish` rolls such an entry back to COMMITTED on disconnect, and the peer may retransmit a fulfill instead. Once the inbound fail is on the wire it cannot be retracted, and `canFulfillHtlc` refuses the FAILED inbound entry, so the late preimage is worthless upstream. The same invariant this PR enforces in the deferred paths (never refund upstream while the downstream can still claim) is not enforced on the message-driven path.

## Reproduction

1. Alice forwards Bob -> Carol; Carol is the payee and holds the preimage. 2. Carol sends update_fail_htlc to Alice, then withholds her commitment_signed so the removal round never completes. 3. Alice's `handleUpdateFailHtlc` sets the offered entry FAILED (both removal flags false) and emits HTLC_FAILED; `handleHtlcFailed` immediately fails the inbound leg to Bob and deletes the linkage. 4. The Alice-Bob removal round completes; Alice can no longer collect upstream. 5. Carol disconnects; `markForReestablish` rolls the offered leg back to COMMITTED. 6. Carol reconnects and retransmits update_fulfill_htlc with the preimage; Alice accepts it and pays Carol. Actual result: Alice pays Carol and has already refunded Bob; the preimage is unusable upstream (the linkage is gone and `canFulfillHtlc` refuses the FAILED inbound entry). The channel-layer rollback and fulfill acceptance in steps 5-6 are correct per BOLT 2; the defect is the immediate upstream refund in step 3.

## Impact

A malicious downstream payee can steal the forward amount from a forwarding node: fail the HTLC, wait for the upstream refund to complete, then disconnect mid-removal and retransmit the fulfill after reestablish. The node pays downstream for a payment it already refunded upstream, with no on-chain recourse. The window is fully attacker-controlled, since withholding commitment_signed holds the removal round open indefinitely.

## Expected behavior

A forwarded inbound leg must not be failed upstream until the outgoing leg is terminally failed under the same `isOutgoingLegTerminallyFailed` predicate the deferred paths use (both removal flags past false, entry gone from a live NORMAL channel, or monitor-proven on-chain resolution). The downstream fail's reason bytes must survive until then, or a synthesized failure used as `settleForwardsOwedUpstream` already does. A stalled downstream removal must still resolve via the existing force-close fallback at the forward-timeout margin. This behaviour predates this branch.

Found while working on #590.

## [non-blocking] Older scans overwrite newer watch results

Older asynchronous chain scans can overwrite newer spend and funding-presence verdicts.

## Problem

`checkOutputSpend` guards only lifecycle generation and watch identity, which are shared by overlapping scans. `discoverRestoredFunding` mutates `watched.provisional` before `checkFundingConfirmation` applies its scan-ticket check.

## Reproduction

1. Pause an old output-spend scan in `getTransaction`.
2. Complete a newer post-reorg scan with empty history.
3. Resume the old scan.
4. Repeat with restored-funding discovery, letting a newer scan record absence before the old discovery resumes.

Actual result:
- Output events are `unspent -> spent`.
- Funding presence changes from `absent` to `present`.

## Impact

A vanished spend can be restored as current, eventually retiring its watch and suppressing claim rebroadcast. Stale provisional funding can also clear a missing-funding quarantine or forget clock.

## Expected behavior

This behavior predates the branch. Add per-watch scan-order arbitration before any state mutation, including provisional funding updates. Ignore older completions and add interleaving tests for both paths.

Found while working on #592.

