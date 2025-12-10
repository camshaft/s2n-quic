# s2n-quic-dc Documentation

This directory contains the s2n-quic-dc crate implementation and documentation.

## Documentation Overview

**Quick Start**: See `QUICK_REFERENCE.md` for a one-page overview!

### Technical Specifications
- **`udp-improvements.md`** - Detailed technical specification of UDP send path improvements
  - 14 work items with implementation details
  - Reasoning for design decisions
  - Status tracking table

- **`DIAGRAMS.md`** - Visual architecture diagrams
  - Current vs new architecture
  - Work item dependencies
  - Implementation timeline
  - Data flow sequences
  - Component interactions

### Project Management
- **`INVESTIGATION_SUMMARY.md`** - High-level overview and investigation results
  - Executive summary
  - Architecture comparison (before/after)
  - Implementation phases
  - Risk assessment
  - Team coordination plan
  - **START HERE** for overview

- **`WORK_ITEM_STATUS.md`** - Status tracking and task assignment
  - Status of each work item
  - Dependencies between items
  - Assignments (to be filled in)
  - Progress tracking
  - Meeting notes

### Testing
- **`TEST_COVERAGE_PLAN.md`** - Comprehensive test plans
  - 250+ planned tests
  - Unit, integration, and E2E tests
  - Test specifications for each work item
  - Testing infrastructure requirements
  - Success criteria

## Quick Start

### For Team Leads
1. Read `INVESTIGATION_SUMMARY.md` for overview
2. View `DIAGRAMS.md` for visual architecture
3. Review `WORK_ITEM_STATUS.md` for current status
4. Assign work items to team members
5. Track progress in `WORK_ITEM_STATUS.md`

### For Developers
1. Read `INVESTIGATION_SUMMARY.md` for context
2. View `DIAGRAMS.md` for visual understanding
3. Find your assigned work item in `WORK_ITEM_STATUS.md`
4. Read the detailed specification in `udp-improvements.md`
5. Review test plans in `TEST_COVERAGE_PLAN.md`
6. Implement code and tests together

### For Reviewers
1. Check `WORK_ITEM_STATUS.md` for item being reviewed
2. Reference `udp-improvements.md` for expected behavior
3. Verify tests from `TEST_COVERAGE_PLAN.md` are implemented
4. Update status when approved

## Work Item Dependencies

Work items must be completed in order due to dependencies:

**Phase 1 (Foundation)**: #1 → #12 → #2 → #4  
**Phase 2 (Core)**: #3 → #7 → #6  
**Phase 3 (Features)**: #5 → #8, #10, #11  
**Phase 4 (Polish)**: #13, #9, #14

See `WORK_ITEM_STATUS.md` for detailed dependency graph.

## Status Legend

- 🔴 **Not Started**: Work has not begun
- 🟡 **In Progress**: Work is underway
- 🟢 **Complete**: Work is done and tested
- 🔵 **Blocked**: Cannot proceed due to dependency
- ⚪ **Skipped**: Decided not to implement

## Contributing

When working on a work item:

1. Update status in `WORK_ITEM_STATUS.md` to 🟡 In Progress
2. Implement code changes as specified in `udp-improvements.md`
3. Implement tests as specified in `TEST_COVERAGE_PLAN.md`
4. Run tests: `cargo test -p s2n-quic-dc`
5. Get code review
6. Update status in `WORK_ITEM_STATUS.md` to 🟢 Complete
7. Update test coverage percentage

## Testing

```bash
# Run all tests
cargo test -p s2n-quic-dc

# Run specific work item tests
cargo test -p s2n-quic-dc <test_name>

# Run with coverage
cargo tarpaulin -p s2n-quic-dc --out Html

# Run benchmarks
cargo bench -p s2n-quic-dc-benches
```

## Questions?

- Architecture questions: See `DIAGRAMS.md` for visual explanations
- Technical questions: See `udp-improvements.md` for detailed reasoning
- Status questions: Check `WORK_ITEM_STATUS.md`
- Test questions: See `TEST_COVERAGE_PLAN.md`
- General questions: See `INVESTIGATION_SUMMARY.md`

If you can't find the answer, add it to the "Questions & Issues" section in `WORK_ITEM_STATUS.md`.
