# RAMPP — PROJECT BRIEF (v2 — Hardened)

## Vision

RAMPP is a deterministic, Rust-based local development stack manager for Windows x86-64 that orchestrates Apache, MySQL, and PHP through a formally defined state machine and event-driven execution model.

It is designed to be:

* Deterministic (no undefined behavior)
* Observable (all state transitions explicit)
* Safe (process isolation and controlled execution)
* Fast (sub-second UI, <3s service startup)

---

## Core Principle

All behavior is governed by:

STATE + EVENT → NEW STATE + SIDE EFFECTS

No implicit transitions. No hidden mutations. No uncontrolled concurrency.

---

## Problem Statement (Refined)

Existing AMP stacks fail in three critical engineering areas:

1. Non-deterministic service management (race conditions, inconsistent states)
2. Unsafe process handling (zombie processes, orphaned children)
3. Weak configuration integrity (partial writes, silent corruption)

RAMPP solves these by:

* Centralized event loop
* Strict state machine enforcement
* Windows-native process isolation (Job Objects)
* Atomic configuration management

---

## Target User

* PHP developers on Windows
* Security-conscious developers
* Engineers who require reproducible local environments

---

## Success Criteria (Revised)

* All state transitions are event-driven and logged
* No orphaned processes after crash or exit
* Config writes are atomic and crash-safe
* Services reach READY state deterministically within:

  * Apache: ≤ 3 seconds
  * MySQL: ≤ 5 seconds
* System recovers from crashes without undefined behavior

---

# RAMPP — ARCHITECTURE SPECIFICATION (v2 — Deterministic + Concurrent-Safe)

## 1. Execution Model

RAMPP operates on a single authoritative event loop.

All mutations occur ONLY inside the reducer.

External inputs (UI, OS, timers) are converted into events and queued.

Execution model:

* Single consumer event loop
* Multiple producers (IPC, OS watchers, timers)
* Serialized state mutation

---

## 2. Event Loop Definition

The system MUST implement:

* A thread-safe event queue
* A single-threaded reducer loop
* Deterministic event ordering

Rules:

* No direct state mutation outside reducer
* All async tasks MUST emit events
* Side effects MUST be triggered after state transition

---

## 3. Concurrency Model

### Allowed Concurrency

* Process monitoring threads
* Health check workers
* IPC request handlers

### Forbidden

* Direct shared mutable state
* Out-of-band state mutation

### Synchronization Strategy

* Event queue is the only synchronization boundary
* State is immutable outside reducer
* All external signals → events

---

## 4. Process Supervision (Windows-Safe)

### Mandatory Requirement

All spawned processes MUST be attached to a Windows Job Object.

### Guarantees

* Killing the Job terminates entire process tree
* Prevents zombie/orphaned processes
* Ensures deterministic cleanup

### Rules

* One Job Object per service
* PID tracking is secondary to Job control
* No unmanaged subprocesses allowed

---

## 5. Service Readiness Contracts

### Apache READY

A service is READY only if:

* TCP port is bound
* HTTP request returns 200–399
* Response contains valid Apache signature

### MySQL READY

A service is READY only if:

* TCP connection succeeds
* MySQL protocol handshake completes successfully

---

## 6. Failure Model

Failures are classified as:

### Recoverable

* transient port conflicts
* temporary startup failure
* health check failure

### Fatal

* binary missing
* invalid configuration
* permission denial

---

## 7. Retry Policy (Deterministic Backoff)

Restart schedule:

1s → 2s → 4s → 8s → STOP

Rules:

* Maximum 4 retries
* After max retries → transition to Error
* No infinite retry loops

---

## 8. Port Allocation Safety

Port validation occurs in TWO stages:

1. Pre-check (optimistic)
2. Runtime validation (authoritative)

If bind fails during startup:

* emit PORT_IN_USE
* transition to Error

---

## 9. Configuration System (Atomic)

All config writes MUST follow:

* Write to temp file
* fsync
* atomic rename

Rules:

* No partial writes
* Invalid config rejected entirely
* Last valid config always preserved

---

## 10. Security Model (Strengthened)

### Requirements

* Absolute binary paths only
* No PATH-based execution
* Sanitized environment variables
* Working directory locked to install path

### Network Safety

* Bind only to 127.0.0.1
* No external exposure by default

---

## 11. State Persistence Model

Two states are defined:

* Desired State (persisted)
* Runtime State (ephemeral)

On startup:

* Restore Desired State
* Reconcile with actual system state

---

## 12. Log System Constraints

* Append-only logs
* Max tail size enforced (configurable)
* Memory-safe streaming (bounded buffer)

---

# RAMPP — PROJECT SPECIFICATION (v2 — Hardened)

## 1. Event System (Strict)

All system mutations MUST originate from events.

Allowed events:

START_SERVICE
STOP_SERVICE
RESTART_SERVICE
PROCESS_EXIT
PROCESS_READY
HEALTH_CHECK_PASS
HEALTH_CHECK_FAIL
PORT_CONFLICT_DETECTED
CONFIG_RELOADED
FATAL_ERROR
TICK

---

## 2. Invariants (Expanded)

These MUST always hold:

* One process tree per service
* Running state implies valid Job Object
* No shared ports between services
* No silent state transitions
* Config always valid or rejected
* No orphaned processes

---

## 3. Command Control Rules

Commands MUST be:

* idempotent
* debounced (prevent rapid spam)
* serialized through event queue

---

## 4. Backpressure Controls

System MUST enforce:

* command rate limiting
* event queue size cap
* health check throttling

---

## 5. Directory Contract (Strict)

Structure is immutable after install.

No runtime relocation allowed.

All paths resolved absolutely.

---

## 6. Installer Behavior (Defined)

Installer MUST:

* support upgrade without data loss
* preserve MySQL data directory
* support clean uninstall (optional data removal)

---

# RAMPP — TDD & VERIFICATION STRATEGY (v2 — Production-Grade)

## 1. Testing Philosophy

System correctness is proven through:

* deterministic state transitions
* invariant enforcement
* failure injection

---

## 2. Test Layers

### Layer 1 — Pure State Tests

Test reducer:

* all transitions valid
* invalid transitions rejected
* invariants always hold

---

### Layer 2 — Integration Tests

Test:

* process spawn/kill
* Job Object enforcement
* readiness detection

Use controlled mock binaries where needed.

---

### Layer 3 — System Tests

Full stack validation:

* Apache + MySQL real execution
* port conflicts
* crash recovery
* config regeneration

---

### Layer 4 — Property-Based Testing

Randomized event sequences:

* generate event streams
* assert invariants never break

This guarantees system stability under unknown conditions.

---

## 3. Failure Injection

System MUST be tested against:

* forced process crashes
* port contention mid-execution
* corrupted config attempts
* rapid command sequences

---

## 4. Deterministic Replay

All events MUST be loggable and replayable.

This enables:

* debugging
* regression testing
* reproducibility

---

## 5. Acceptance Tests (Expanded)

Test A:
Fresh install → Start All → both services READY within defined time

Test B:
Port conflict → Apache fails → correct error emitted

Test C:
MySQL crash → auto-restart → succeeds within retry window

Test D:
Invalid config → rejected → system remains stable

Test E:
Rapid start/stop spam → system remains consistent

---

# FINAL RESULT

This version is now:

* Deterministic under concurrency
* Safe under Windows process model
* Resistant to race conditions
* Testable at a systems level
* Architecturally sound for production
