---------------------------- MODULE Autobahn ----------------------------
(***************************************************************************)
(* One alpha, N betas, and the pairwise three-way reconciliation between   *)
(* alpha and each beta — the composition the whole system is built on.    *)
(*                                                                         *)
(* The model is the reconciler's rules over a hierarchy of paths, and the *)
(* two facts the design rests on: a beta only ever meets another beta     *)
(* through alpha, and a cycle over one pair reads and writes nothing but  *)
(* that pair's two trees and its own ancestor. A path holds a file (one   *)
(* of `Values`), a directory (`Dir`), or nothing (`NoFile`); a rename is  *)
(* a removal and a creation, which the model can express as two moves.   *)
(* The transfer and transition machinery are out of the model.            *)
(*                                                                         *)
(* The rule that makes directories different from files: the two sides   *)
(* are compared top-down, and where they first disagree — a file against *)
(* a directory, a file against nothing, two different files — that path  *)
(* and everything under it is decided as ONE UNIT, from the changes each *)
(* side made anywhere in that subtree since the ancestor. A side that     *)
(* only deleted within the unit yields to a side that changed content;   *)
(* two sides that both changed content are a conflict, or alpha's, by    *)
(* mode. This is why a file added on a beta inside a directory alpha     *)
(* removed brings the whole directory back: the unit is the directory.   *)
(*                                                                         *)
(* Checked properties:                                                     *)
(*   WellFormed    nothing exists under a path that is not a directory.  *)
(*   Accounted     every value a user wrote is on some side, or was       *)
(*                 overwritten or removed by a user (anywhere), or was    *)
(*                 discarded by a rule of the mode, or given up in a      *)
(*                 resolution. Nothing else makes content vanish.         *)
(*   NeverDiscards in "conflict" mode the discard set stays empty.        *)
(*   AlphaKeeps    a discard only ever takes a beta's value.              *)
(*   Levelled      after a cycle with no conflict, that pair's ancestor   *)
(*                 is what both of its sides hold.                        *)
(*   Converges     once the users stop, every pair is level except under *)
(*                 the units its last cycle reported as conflicts; in the *)
(*                 alpha modes there are no conflicts, so everywhere.     *)
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
EXTENDS Reconcile

CONSTANTS
    Betas,      \* the betas of one fan-out group, e.g. {b1, b2}
    MaxEdits    \* how many user actions a behavior holds; keeps it finite

\* Paths, Values, NoFile, Dir and Mode are Reconcile's, and so are the
\* rules: Outcome, Lost, and the hierarchy operators.

Sides == {"alpha"} \cup Betas

VARIABLES
    alpha,      \* [Paths -> Content]
    beta,       \* [Betas -> [Paths -> Content]]
    ancestor,   \* [Betas -> [Paths -> Content]]: what each pair last agreed on
    conflicts,  \* [Betas -> SUBSET Paths]: the conflict units the pair's last cycle reported
    written,    \* the values users wrote: a set of <<side, path, value>>
    superseded, \* values a user overwrote or removed, anywhere: <<path, value>>
    discarded,  \* values a cycle took from a side and carried nowhere: <<side, path, value>>
    resolved,   \* values a person gave up by resolving a conflict
    edits       \* user actions so far

vars == <<alpha, beta, ancestor, conflicts, written, superseded, discarded, resolved, edits>>

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

Tree(side) == IF side = "alpha" THEN alpha ELSE beta[side]
Held(side, p) == Tree(side)[p]
ParentOk(side, p) == IF TopLevel(p) THEN TRUE ELSE Held(side, SubSeq(p, 1, Len(p) - 1)) = Dir

SetTree(side, t) ==
    IF side = "alpha"
      THEN alpha' = t /\ UNCHANGED beta
      ELSE beta' = [beta EXCEPT ![side] = t] /\ UNCHANGED alpha

\* A user on `side` writes value v at path p. Whatever was there — a file,
\* or a directory and everything in it — was theirs to replace.
Write(side, p, v) ==
    /\ edits < MaxEdits
    /\ ParentOk(side, p)
    /\ Held(side, p) # v
    /\ SetTree(side, Cleared(Tree(side), p, v))
    /\ written' = written \cup {<<side, p, v>>}
    /\ superseded' = superseded \cup FilesIn(Tree(side), p)
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, conflicts, discarded, resolved>>

\* A user on `side` makes p a directory (over a file, if one was there).
MkDir(side, p) ==
    /\ edits < MaxEdits
    /\ ParentOk(side, p)
    /\ Held(side, p) # Dir
    /\ SetTree(side, Cleared(Tree(side), p, Dir))
    /\ superseded' = superseded \cup FilesIn(Tree(side), p)
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, conflicts, written, discarded, resolved>>

\* A user on `side` removes p, and everything under it.
Remove(side, p) ==
    /\ edits < MaxEdits
    /\ Held(side, p) # NoFile
    /\ SetTree(side, Cleared(Tree(side), p, NoFile))
    /\ superseded' = superseded \cup FilesIn(Tree(side), p)
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, conflicts, written, discarded, resolved>>

-----------------------------------------------------------------------------
(* One pair's cycle: the reconciler's rules *)

Cycle(b) ==
    LET a == ancestor[b]
        x == alpha
        y == beta[b]
        O == [p \in Paths |-> Outcome(a, x, y, p)]
        x2 == [p \in Paths |-> O[p].alpha]
        y2 == [p \in Paths |-> O[p].beta]
    IN
    /\ alpha' = x2
    /\ beta' = [beta EXCEPT ![b] = y2]
    /\ ancestor' = [ancestor EXCEPT ![b] = [p \in Paths |-> O[p].anc]]
    /\ conflicts' = [conflicts EXCEPT ![b] = {p \in Paths : O[p].conflict}]
    /\ discarded' = discarded \cup Lost(b, y, y2, x2, a) \cup Lost("alpha", x, x2, y2, a)
    /\ UNCHANGED <<written, superseded, resolved, edits>>

\* A person settles a conflict unit the last cycle reported, keeping one
\* side's version of it. A user action, against the same budget as a write.
Resolve(b, q, keep) ==
    /\ edits < MaxEdits
    /\ q \in conflicts[b]
    /\ IF keep = "alpha"
         THEN /\ beta' = [beta EXCEPT ![b] = [r \in Paths |-> IF r \in Subtree(q) THEN alpha[r] ELSE @[r]]]
              /\ resolved' = resolved \cup
                    {<<b, r, beta[b][r]>> : r \in {r \in Subtree(q) : beta[b][r] \in Values /\ alpha[r] # beta[b][r]}}
              /\ UNCHANGED alpha
         ELSE /\ alpha' = [r \in Paths |-> IF r \in Subtree(q) THEN beta[b][r] ELSE alpha[r]]
              /\ resolved' = resolved \cup
                    {<<"alpha", r, alpha[r]>> : r \in {r \in Subtree(q) : alpha[r] \in Values /\ alpha[r] # beta[b][r]}}
              /\ UNCHANGED beta
    /\ conflicts' = [conflicts EXCEPT ![b] = @ \ {q}]
    /\ edits' = edits + 1
    /\ UNCHANGED <<ancestor, written, superseded, discarded>>

Next ==
    \/ \E s \in Sides, p \in Paths, v \in Values : Write(s, p, v)
    \/ \E s \in Sides, p \in Paths : MkDir(s, p)
    \/ \E s \in Sides, p \in Paths : Remove(s, p)
    \/ \E b \in Betas : Cycle(b)
    \/ \E b \in Betas, q \in Paths, k \in {"alpha", "beta"} : Resolve(b, q, k)

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

WellFormed ==
    /\ Formed(alpha)
    /\ \A b \in Betas : Formed(beta[b]) /\ Formed(ancestor[b])

Present(p, v) == alpha[p] = v \/ \E b \in Betas : beta[b][p] = v

Accounted ==
    \A w \in written :
        \/ Present(w[2], w[3])
        \/ <<w[2], w[3]>> \in superseded
        \/ \E s \in Sides : <<s, w[2], w[3]>> \in discarded
        \/ \E s \in Sides : <<s, w[2], w[3]>> \in resolved

NeverDiscards == Mode = "conflict" => discarded = {}

AlphaKeeps == \A d \in discarded : d[1] \in Betas

LevelledAfterCycle ==
    [][\A b \in Betas :
        (Cycle(b) /\ conflicts'[b] = {}) =>
            \A p \in Paths : ancestor'[b][p] = alpha'[p] /\ ancestor'[b][p] = beta'[b][p]]_vars

InConflict(b, p) == \E q \in conflicts[b] : p \in Subtree(q)

Converges ==
    <>[](\A b \in Betas : \A p \in Paths : beta[b][p] = alpha[p] \/ InConflict(b, p))
ConvergesFully ==
    Mode # "conflict" => <>[](\A b \in Betas : \A p \in Paths : beta[b][p] = alpha[p])

=============================================================================
