# Bitneedle Picture Record Format (draft-04) — Fixes

## Fatal — blocks from-scratch implementation
- [ ] Define toned-v1 exact reversible symbol ordering + pixel selection in-document (or reference a located, versioned spec / contract)
- [ ] Add concrete binary test vectors — Appendix A is currently structural summaries only, no bytes

## High-risk
- [ ] Make exact-digital guarantee determinable: cos/sin/tanh differ across libm; pin reference libm or correctly-rounded algos, rounding mode, FMA policy
- [ ] Locate or scope-out ECDC/MOSSNANO specs; release commitment (section 12.2) hashes "ECDC entry bytes" but ECDC is only a name

## Recommended test vectors
- [ ] One complete BRD1 stream with expected CRC-32
- [ ] One BRS1 metadata header + chunk set with CRC (+ optional signature)
- [ ] splitmix64 first-N outputs; Mulberry32 first-N outputs (carrier shuffle)
- [ ] GAP1 xorshift32 filler keystream for a declared seed
- [ ] Short worked spiral: first ~10 traced pixel coordinates (Archimedean + one vari-pitch set)
