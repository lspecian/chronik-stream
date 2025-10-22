# Chronik Documentation

This directory contains all documentation, planning documents, and session summaries for the Chronik project.

## Directory Structure

```
docs/
├── README.md                    # This file
├── sessions/                    # Development session summaries
├── planning/                    # Planning and tracking documents
├── DOCKER.md                    # Docker deployment guide
├── DISASTER_RECOVERY.md         # DR guide (v1.3.65+)
├── KSQL_INTEGRATION_GUIDE.md    # KSQL integration
└── ... (other guides)
```

## Session Summaries (`sessions/`)

Historical development session summaries documenting progress, decisions, and learnings:

- `DAY1_SUMMARY.md` - Initial Raft implementation session
- `DAY2_CONT_SESSION_SUMMARY.md` - Day 2 continued work
- `DAY2_FINAL_SUMMARY.md` - Day 2 completion
- `SESSION_SUMMARY_2025-10-19.md` - Detailed session notes
- `SESSION_SUMMARY_2025-10-19_CONTINUED.md` - Continuation
- `SESSION_SUMMARY_2025-10-21_FINAL.md` - Final Phase 2 work
- `SESSION_SUMMARY_2025-10-21_PHASE2.md` - Phase 2 summary
- `PHASE2_COMPLETE_SUMMARY.md` - Phase 2 completion report

**Purpose**: Historical record of development decisions, bugs discovered, fixes applied, and lessons learned.

## Planning Documents (`planning/`)

Feature tracking, task breakdowns, and investigation reports:

- `CLUSTERING_TRACKER.md` - Raft clustering feature tracker
- `CLUSTERING_COMPLETION_ROADMAP.md` - Roadmap to production-ready clustering
- `RAFT_IMPLEMENTATION_SUMMARY.md` - Raft implementation overview
- `TASK_2.1_SUMMARY.md` - Task 2.1 completion notes
- `TASK_2.2_SUMMARY.md` - Task 2.2 completion notes
- `TASK_2.4_SUMMARY.md` - Task 2.4 completion notes
- `INVESTIGATION.md` - Technical investigations
- `METRICS_ISSUE.md` - Metrics system debugging

**Purpose**: Track feature development, plan implementation phases, and document technical investigations.

## User Guides

### DOCKER.md
Complete guide to Docker deployment, including:
- Image building
- Environment variables
- Container orchestration
- Troubleshooting

### DISASTER_RECOVERY.md
Disaster recovery guide (v1.3.65+):
- Metadata backup to S3/GCS/Azure
- Cold start recovery procedures
- Restore verification
- Configuration examples

### KSQL_INTEGRATION_GUIDE.md
KSQL integration documentation:
- Setup instructions
- Stream creation examples
- Compatibility notes
- Troubleshooting

## Document Organization Guidelines

### When to Add Session Summaries
- After significant feature completion
- When major bugs are discovered/fixed
- For architectural decisions that need documentation
- End of major development sessions

### When to Add Planning Documents
- Starting new feature development
- Creating task breakdowns
- Technical investigations requiring notes
- Feature tracking across multiple sessions

### File Naming Conventions
- Session summaries: `SESSION_SUMMARY_YYYY-MM-DD[_DESCRIPTION].md`
- Planning docs: `[FEATURE]_[TYPE].md` (e.g., `CLUSTERING_TRACKER.md`)
- User guides: `[TOPIC].md` (e.g., `DOCKER.md`)

## Key Documentation

For the most up-to-date project information, see:

- **[Main README](../README.md)** - Project overview, features, quick start
- **[CLAUDE.md](../CLAUDE.md)** - Development guidelines, architecture, testing
- **[CHANGELOG.md](../CHANGELOG.md)** - Version history and release notes
- **[CONTRIBUTING.md](../CONTRIBUTING.md)** - Contribution guidelines

## Documentation History

### Major Documentation Updates

**v1.3.66 (2025-10-22)**
- Reorganized project structure (moved test scripts, session summaries, planning docs)
- Created comprehensive test scripts README
- Updated documentation organization

**v1.3.65 (2025-10-21)**
- Added disaster recovery documentation
- Documented metadata backup feature
- Cold start recovery procedures

**v1.3.64 (2025-10-20)**
- Layered storage architecture documentation
- Multi-tier fetch mechanism
- S3 configuration examples

**Earlier versions**
- See individual session summaries for historical documentation changes
