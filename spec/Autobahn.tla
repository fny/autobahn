---------------------------- MODULE Autobahn ----------------------------
(***************************************************************************)
(* One alpha, N betas, and the pairwise three-way reconciliation between   *)
(* alpha and each beta — the composition the whole system is built on.    *)
(*                                                                         *)
(* The model is the reconciler's rules for files, one path at a time, and *)
(* the two facts the design rests on: a beta only ever meets another beta *)
(* through alpha, and a cycle over one pair reads and writes nothing but  *)
(* that pair's two trees and its own ancestor. Directories, renames and   *)
(* the transfer machinery are out of the model; a rename is a deletion    *)
(* and a creation, which the model can express as two writes.             *)
(*                                                                         *)
(* Checked properties:                                                     *)
(*   Accounted     every value a user wrote is on some side, or was       *)
(*                 overwritten by that same user, or was discarded by a   *)
(*                 rule of the mode that names the side it took it from.  *)
(*   NeverDiscards in "conflict" mode the discard set stays empty: the    *)
(*                 mode's promise that nothing is lost, ever.             *)
(*   AlphaKeeps    a discard only ever takes a beta's value: alpha never  *)
(*                 loses to a beta in the alpha modes.                    *)
(*   Levelled      after a cycle with no conflict, that pair's ancestor   *)
(*                 is what both of its sides hold.                        *)
(*   Converges     once the users stop, every pair is level except at    *)
(*                 the paths its last cycle reported as conflicts, which  *)
(*                 stay reported until a person acts (liveness). In the   *)
(*                 alpha modes there are no conflicts, so every beta      *)
(*                 comes to hold what alpha holds.                        *)
(*                                                                         *)
(* One behavior TLC found is worth knowing: with resolution unbounded,    *)
(* two betas holding different content for one path can be "resolved"    *)
(* in their own favor alternately forever — alpha flips between the two  *)
(* and neither pair levels. A resolver sees two sides, never the third   *)
(* machine. So a resolution counts as a user action here, against the    *)
(* same budget as a write.                                                *)
(*                                                                         *)
(* The Rust implementation is held to this spec by tests/spec_replay.rs:  *)
(* the real reconciler drives the same actions, the same invariants are   *)
(* asserted in Rust, and the runs are written out as traces that TLC      *)
(* validates against this module (see spec/check.sh --traces).            *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
    Betas,      \* the betas of one fan-out group, e.g. {b1, b2}
    Paths,      \* the files that exist in the model, e.g. {p1, p2}
    Values,     \* the contents a user can write, e.g. {v1, v2}
    NoFile,     \* the content of a path that does not exist
    Mode,       \* "conflict" | "alpha" | "strict"
    MaxEdits    \* how many user actions a behavior holds; keeps it finite

ASSUME Mode \in {"conflict", "alpha", "strict"}
ASSUME NoFile \notin Values

Content == Values \cup {NoFile}
Sides == {"alpha"} \cup Betas

VARIABLES
    alpha,      \* [Paths -> Content]
    beta,       \* [Betas -> [Paths -> Content]]
    ancestor,   \* [Betas -> [Paths -> Content]]: what each pair last agreed on
    conflicts,  \* [Betas -> SUBSET Paths]: what the pair's last cycle reported
    written,    \* the values users wrote: a set of <<side, path, value>>
    superseded, \* values a user overwrote or removed, anywhere: <<path, value>>
    discarded,  \* writes a mode rule took from a beta: <<beta, path, value>>
    resolved,   \* writes a person gave up by resolving a conflict
    edits       \* user actions so far

vars == <<alpha, beta, ancestor, conflicts, written, superseded, discarded, resolved, edits>>

Empty == [p \in Paths |-> NoFile]

Init ==
    /\ alpha = Empty
    /\ beta = [b \in Betas |-> Empty]
    /\ ancestor = [b \in Betas |-> Empty]
    /\ conflicts = [b \in Betas |-> {}]
    /\ written = {}
    /\ superseded = {}
    /\ discarded = {}
    /\ resolved = {}
    /\ edits = 0

-----------------------------------------------------------------------------
(* Users *)

Held(side, p) == IF side = "alpha" THEN alpha[p] ELSE beta[side][p]

\* A user on `side` writes value v at path p. Whatever they had there is
\* theirs to overwrite.
Write(side, p, v) ==
    /\ edits < MaxEdits
    /\ Held(side, p) # v
    /\ IF side = "alpha"
         THEN alpha' = [alpha EXCEPT ![p] = v] /\ UNCHANGED beta
         ELSE beta' = [beta EXCEPT ![side][p] = v] /\ UNCHANGED alpha
    /\ written' = written \cup {<<side, p, v>>}
    /\ superseded' = IF Held(side, p) = NoFile THEN superseded
                     ELSE superseded \cup {<<p, Held(side, p)>>}
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, conflicts, discarded, resolved>>

\* A user on `side` removes the file at p.
Remove(side, p) ==
    /\ edits < MaxEdits
    /\ Held(side, p) # NoFile
    /\ IF side = "alpha"
         THEN alpha' = [alpha EXCEPT ![p] = NoFile] /\ UNCHANGED beta
         ELSE beta' = [beta EXCEPT ![side][p] = NoFile] /\ UNCHANGED alpha
    /\ superseded' = superseded \cup {<<p, Held(side, p)>>}
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, conflicts, written, discarded, resolved>>

-----------------------------------------------------------------------------
(* One pair's cycle: the reconciler's rules for a file, per path *)

\* The outcome at one path, given the pair's ancestor a, alpha's x and the
\* beta's y: the new alpha, the new beta, the new ancestor, whether it is
\* a conflict, and whether beta's value was discarded by the mode.
Level(a, x, y) ==
    IF x = y THEN
        [alpha |-> x, beta |-> y, anc |-> x, conflict |-> FALSE, lost |-> FALSE]
    ELSE IF y = a THEN                    \* only alpha changed: it propagates
        [alpha |-> x, beta |-> x, anc |-> x, conflict |-> FALSE, lost |-> FALSE]
    ELSE IF x = a THEN                    \* only beta changed: it propagates
        [alpha |-> y, beta |-> y, anc |-> y, conflict |-> FALSE, lost |-> FALSE]
    ELSE IF x = NoFile THEN               \* alpha deleted, beta edited
        IF Mode = "strict"
          THEN [alpha |-> x, beta |-> x, anc |-> x, conflict |-> FALSE, lost |-> TRUE]
          ELSE [alpha |-> y, beta |-> y, anc |-> y, conflict |-> FALSE, lost |-> FALSE]
    ELSE IF y = NoFile THEN               \* beta deleted, alpha edited: restored
        [alpha |-> x, beta |-> x, anc |-> x, conflict |-> FALSE, lost |-> FALSE]
    ELSE                                  \* both edited
        IF Mode = "conflict"
          THEN [alpha |-> x, beta |-> y, anc |-> a, conflict |-> TRUE, lost |-> FALSE]
          ELSE [alpha |-> x, beta |-> x, anc |-> x, conflict |-> FALSE, lost |-> TRUE]

Cycle(b) ==
    LET L == [p \in Paths |-> Level(ancestor[b][p], alpha[p], beta[b][p])] IN
    /\ alpha' = [p \in Paths |-> L[p].alpha]
    /\ beta' = [beta EXCEPT ![b] = [p \in Paths |-> L[p].beta]]
    /\ ancestor' = [ancestor EXCEPT ![b] = [p \in Paths |-> L[p].anc]]
    /\ conflicts' = [conflicts EXCEPT ![b] = {p \in Paths : L[p].conflict}]
    /\ discarded' = discarded \cup {<<b, p, beta[b][p]>> : p \in {q \in Paths : L[q].lost}}
    /\ UNCHANGED <<written, superseded, resolved, edits>>

\* A person settles a conflict the last cycle reported, keeping one side.
\* A user action, against the same budget as a write (see the note above).
Resolve(b, p, keep) ==
    /\ edits < MaxEdits
    /\ p \in conflicts[b]
    /\ alpha[p] # beta[b][p]
    /\ IF keep = "alpha"
         THEN /\ beta' = [beta EXCEPT ![b][p] = alpha[p]]
              /\ resolved' = resolved \cup {<<b, p, beta[b][p]>>}
              /\ UNCHANGED alpha
         ELSE /\ alpha' = [alpha EXCEPT ![p] = beta[b][p]]
              /\ resolved' = resolved \cup {<<"alpha", p, alpha[p]>>}
              /\ UNCHANGED beta
    /\ conflicts' = [conflicts EXCEPT ![b] = @ \ {p}]
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, written, superseded, discarded>>

Next ==
    \/ \E s \in Sides, p \in Paths, v \in Values : Write(s, p, v)
    \/ \E s \in Sides, p \in Paths : Remove(s, p)
    \/ \E b \in Betas : Cycle(b)
    \/ \E b \in Betas, p \in Paths, k \in {"alpha", "beta"} : Resolve(b, p, k)

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ \A b \in Betas : WF_vars(Cycle(b))

-----------------------------------------------------------------------------
(* Properties *)

TypeOK ==
    /\ alpha \in [Paths -> Content]
    /\ beta \in [Betas -> [Paths -> Content]]
    /\ ancestor \in [Betas -> [Paths -> Content]]
    /\ conflicts \in [Betas -> SUBSET Paths]
    /\ edits \in 0..MaxEdits

Present(p, v) == alpha[p] = v \/ \E b \in Betas : beta[b][p] = v

\* Every value a user wrote is somewhere, or its fate is on record: a user
\* overwrote or removed it (on whichever side it had reached — a deletion
\* propagates by design), a mode rule discarded it, or a person resolved
\* it away. Nothing else may make content vanish.
Accounted ==
    \A w \in written :
        \/ Present(w[2], w[3])
        \/ <<w[2], w[3]>> \in superseded
        \/ \E s \in Sides : <<s, w[2], w[3]>> \in discarded
        \/ \E s \in Sides : <<s, w[2], w[3]>> \in resolved

\* "conflict" mode discards nothing, ever.
NeverDiscards == Mode = "conflict" => discarded = {}

\* A discard only ever takes a beta's value.
AlphaKeeps == \A d \in discarded : d[1] \in Betas

\* A pair that reported no conflict is levelled: its ancestor is what both
\* sides hold — until a user moves one of them again.
LevelledAfterCycle ==
    [][\A b \in Betas :
        (Cycle(b) /\ conflicts'[b] = {}) =>
            \A p \in Paths : ancestor'[b][p] = alpha'[p] /\ ancestor'[b][p] = beta'[b][p]]_vars

\* Once the users are done, every pair is level except where its last
\* cycle reported a conflict — and in the alpha modes, level everywhere.
Converges ==
    <>[](\A b \in Betas : \A p \in Paths : beta[b][p] = alpha[p] \/ p \in conflicts[b])
ConvergesFully ==
    Mode # "conflict" => <>[](\A b \in Betas : \A p \in Paths : beta[b][p] = alpha[p])

=============================================================================
