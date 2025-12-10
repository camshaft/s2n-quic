# Quick Reference Guide

One-page reference for the UDP Send Path Improvements project.

## 📚 Documentation Map

| Document | Purpose | Audience |
|----------|---------|----------|
| **README.md** | Start here - navigation guide | Everyone |
| **INVESTIGATION_SUMMARY.md** | High-level overview & plan | Leadership, Team Leads |
| **DIAGRAMS.md** | Visual architecture diagrams | Everyone |
| **udp-improvements.md** | Technical specification | Developers |
| **WORK_ITEM_STATUS.md** | Status tracking & assignments | Team Leads, PMs |
| **TEST_COVERAGE_PLAN.md** | Detailed test specifications | Developers, QA |

## 🎯 Project Goals

1. **Investigate** high stream concurrency issues
2. **Document** all changes in the branch
3. **Design** comprehensive test coverage (250+ tests)
4. **Track** status for team coordination
5. **Build** confidence through testing

## 📊 By The Numbers

- **14** Work Items
- **250+** Tests planned
- **8** Weeks estimated
- **4** Implementation phases
- **90%** Target code coverage

## 🔧 Work Items at a Glance

### P0 - Critical (Must do first)
1. Pool Integration
2. Completion Queue Setup
3. Wheel-Based Transmission
4. Pool Refactor
6. Worker Updates
7. State Updates
12. Pool Wiring

### P1 - High Priority
5. Flow Control
8. CCA Pacing
10. Retransmissions
11. ACK Updates
13. GSO Support

### P2 - Medium Priority
9. Configuration
14. Partial Send Removal

## 🚀 Implementation Phases

```
Phase 1 (Week 1-2): Foundation
  └─ Pool integration, wiring, completions, refactor

Phase 2 (Week 3-4): Core Changes
  └─ Wheel integration, state updates, worker updates

Phase 3 (Week 5-6): Features
  └─ Flow control, CCA pacing, retrans, ACKs

Phase 4 (Week 7-8): Polish
  └─ GSO support, configuration, cleanup
```

## 🏗️ Architecture Changes

### Before
```
Application → VecDeque → Direct Socket Send → Network
```
Problems: Custom allocator, no backpressure, naive pacing

### After
```
Application → Pool → Wheel → Socket Worker → Network
     ↑                               ↓
     └── Flow Control ← Completion Queue
```
Benefits: Unified memory, backpressure, CCA pacing, feedback

## 🧪 Test Strategy

| Category | Count | Focus |
|----------|-------|-------|
| Unit Tests | 140 | Component isolation |
| Integration | 70 | Component interactions |
| E2E | 20 | Full send path |
| Performance | 10 | Benchmarks |
| Stress | 10 | High concurrency |

## 📋 Status Legend

- 🔴 Not Started
- 🟡 In Progress
- 🟢 Complete
- 🔵 Blocked
- ⚪ Skipped

## ⚠️ High Risk Areas

1. **Wheel Integration** (#3) - Core architecture change
2. **Pool Refactor** (#4) - Memory management
3. **State Updates** (#7) - Many todo!() calls

**Mitigation**: Extra testing, multiple reviewers, gradual rollout

## 📈 Success Criteria

### Functional
- [ ] All 14 work items complete
- [ ] All todo!() removed
- [ ] No memory leaks
- [ ] 250+ tests passing
- [ ] >90% code coverage

### Performance
- [ ] Throughput maintained or improved
- [ ] Latency P99 <10ms
- [ ] Memory usage stable
- [ ] 1000+ concurrent streams work

## 🔗 Dependencies

```
#1 (Pool) → #4, #12, #11
#2 (Completion) → #3, #5, #6
#3 (Wheel) → #8, #13, #14
#4 + #12 → #7
#7 → #10
#5 → #9
```

## 💡 Quick Commands

```bash
# Run tests
cargo test -p s2n-quic-dc

# Run with coverage
cargo tarpaulin -p s2n-quic-dc --out Html

# Run benchmarks
cargo bench -p s2n-quic-dc-benches

# Specific test
cargo test -p s2n-quic-dc <test_name>
```

## 📝 Team Meeting Checklist

- [ ] Review INVESTIGATION_SUMMARY.md
- [ ] Review DIAGRAMS.md together
- [ ] Assign work items in WORK_ITEM_STATUS.md
- [ ] Set up testing infrastructure
- [ ] Establish baseline metrics
- [ ] Schedule weekly syncs
- [ ] Create Slack/Teams channel

## 🐛 Debugging High Concurrency Issues

Likely causes based on architecture:
1. Memory pressure (custom allocator)
2. No flow control backpressure
3. Naive pacing (yields every 5 packets)
4. Possible lock contention

Solutions being implemented:
- Pool-based memory (fixes #1)
- Flow control with pending tracking (fixes #2)
- CCA-driven pacing via wheel (fixes #3)
- Lock-free intrusive queues (fixes #4)

## 📞 Need Help?

1. **Architecture**: See DIAGRAMS.md
2. **Technical details**: See udp-improvements.md
3. **Status**: Check WORK_ITEM_STATUS.md
4. **Tests**: See TEST_COVERAGE_PLAN.md
5. **Overview**: Read INVESTIGATION_SUMMARY.md

Add questions to WORK_ITEM_STATUS.md "Questions & Issues" section.

## 🎉 Next Steps

1. **Week 1**: Team review, assignments, baseline metrics
2. **Week 2**: Start Phase 1 (Pool Integration)
3. **Weekly**: Progress updates, demos, adjustments
4. **Week 8**: Final validation, benchmarks
5. **Week 10**: Deploy with monitoring

---

**Last Updated**: 2025-12-10  
**Project Status**: Planning Complete - Ready for Implementation

Print this page for your desk or pin in team chat! 📌
