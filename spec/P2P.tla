----------------------------- MODULE P2P -----------------------------
(***************************************************************************)
(* The star with failover: p2p. The reconciliation is Reconcile's,    *)
(* unchanged; what this module adds is who runs a cycle, and what keeps   *)
(* two controllers from ever writing one host.                            *)
(*                                                                         *)
(* Hosts are the alpha and the betas. Every host holds a LEASE — the      *)
(* leader's name and a term — and admits a controller's writes only if   *)
(* the controller presents a term above the lease's, or the same term    *)
(* from the same leader. That refusal, the fence, is the whole safety    *)
(* argument. A controller that is refused steps down and follows.        *)
(*                                                                         *)
(* The alpha leads at first. A beta that finds its lease stale takes     *)
(* over at the next term — in the model, whenever the leader it knows of *)
(* is down, or (with Flaky) at any time at all, which stands for a clock *)
(* that misjudged staleness; the betas take over in their configured     *)
(* order. When a beta leads, the alpha attaches to it and follows, and   *)
(* takes the lead back at the next term once their session has settled. *)
(*                                                                         *)
(* Each (alpha, beta) session has an ancestor. The controller that leads *)
(* the session holds the live one (`own`); it replicates it to the other *)
(* side (`copy`), which may lag. A host that comes to lead a session     *)
(* adopts its copy when the copy was written after its own store, or it  *)
(* has no history of its own — the implementation's adopt_newer_copy,    *)
(* which compares when each was written on the host (`later`). A copy    *)
(* that lagged when a beta took over carries on from where it lagged, so *)
(* generations cannot say which is later. Adoption sets aside every path *)
(* where the copy disagrees with the host's own tree: forgotten, so the  *)
(* next cycle reconciles it as new. (The implementation keeps the copy's *)
(* record where the host's file changed since the copy was written; that *)
(* only keeps an honest record the host has since moved past.) The alpha *)
(* takes the lead back only once its copy is level with the leader's.   *)
(* With Lies, a partner may write any copy at all, a bounded number of   *)
(* times: a buggy or dishonest leader. A session between two betas       *)
(* exists only while a beta leads, and starts without history.           *)
(*                                                                         *)
(* Hosts crash and recover (a bounded number of times); a recovered      *)
(* host comes back as a follower unless its own lease still names it.    *)
(*                                                                         *)
(* Checked, beyond Reconcile's properties carried over:                   *)
(*   Fenced         no host is ever written by two controllers at one    *)
(*                  term, and the term a host is written at never falls. *)
(*   NoResurrection a value removed by a user and gone from every host   *)
(*                  never reappears except by a user writing it again.   *)
(*   Converges      (Flaky = FALSE) once users and failures stop, a      *)
(*                  leader stands and every host it reaches is level     *)
(*                  with it, except under reported conflicts.            *)
(*   DisputedKept   a value a host held where the copy it adopted        *)
(*                  disagreed is never lost: the copy's word never       *)
(*                  overwrites it. With Lies, Accounted is not checked:  *)
(*                  a copy that agrees with a host's tree is believed,   *)
(*                  as a partner editing its own side would be.          *)
(***************************************************************************)
EXTENDS Reconcile

CONSTANTS
    Betas,        \* the betas, e.g. {b1, b2}
    Order,        \* the betas as a sequence: the failover order
    MaxEdits,     \* user actions per behavior
    MaxFailures,  \* crashes per behavior
    MaxChanges,   \* changes of leadership per behavior — terms grow with
                  \* them, so without a bound the state space is infinite
    Flaky,        \* TRUE: a beta may judge a lease stale at any time
    Lies,         \* TRUE: a partner may write a copy that is no record at all
    MaxLies       \* such copies per behavior

ASSUME Mode \in {"conflict", "alpha"}
ASSUME Len(Order) = Cardinality(Betas) /\ {Order[i] : i \in 1..Len(Order)} = Betas

Hosts == {"alpha"} \cup Betas
Earlier(b) == {Order[i] : i \in {i \in 1..Len(Order) : \E j \in 1..Len(Order) : Order[j] = b /\ i < j}}

VARIABLES
    tree,       \* [Hosts -> [Paths -> Content]]
    up,         \* [Hosts -> BOOLEAN]
    lease,      \* [Hosts -> [leader: Hosts, term: Nat]]: the fence at each host
    role,       \* [Hosts -> {"leading", "following"}]: what each controller believes
    myterm,     \* [Hosts -> Nat]: the term a controller presents
    own,        \* [Hosts -> [Betas -> [tree, gen]]]: the (alpha, b) ancestor a host holds as leader
    copy,       \* [Hosts -> [Betas -> [tree, gen]]]: the (alpha, b) ancestor a host holds as a replica
    bb,         \* [Betas -> [Betas -> Tree]]: a leading beta's ancestors with the other betas
    conflicts,  \* [Hosts -> [Hosts -> SUBSET Paths]]: what controller c last reported for host h
    writers,    \* [Hosts -> SUBSET [leader, term]]: every lease under which a host was written
    later,      \* [Hosts -> [Betas -> BOOLEAN]]: the host's copy was written after its own store
    disputed,   \* SUBSET <<host, path, value>>: what a host held where a copy it adopted disagreed
    written, superseded, discarded,
    edits, failures, changes, lies

vars == <<tree, up, lease, role, myterm, own, copy, bb, conflicts, writers, later, disputed, written, superseded, discarded, edits, failures, changes, lies>>

Fresh == [tree |-> Empty, gen |-> 0]

Init ==
    /\ tree = [h \in Hosts |-> Empty]
    /\ up = [h \in Hosts |-> TRUE]
    /\ lease = [h \in Hosts |-> [leader |-> "alpha", term |-> 1]]
    /\ role = [h \in Hosts |-> IF h = "alpha" THEN "leading" ELSE "following"]
    /\ myterm = [h \in Hosts |-> IF h = "alpha" THEN 1 ELSE 0]
    /\ own = [h \in Hosts |-> [b \in Betas |-> Fresh]]
    /\ copy = [h \in Hosts |-> [b \in Betas |-> Fresh]]
    /\ bb = [b \in Betas |-> [c \in Betas |-> Empty]]
    /\ conflicts = [c \in Hosts |-> [h \in Hosts |-> {}]]
    /\ writers = [h \in Hosts |-> {}]
    /\ later = [h \in Hosts |-> [b \in Betas |-> FALSE]]
    /\ disputed = {}
    /\ written = {} /\ superseded = {} /\ discarded = {}
    /\ edits = 0 /\ failures = 0 /\ changes = 0 /\ lies = 0

-----------------------------------------------------------------------------
(* Users, on any host that is up *)

ParentOk(h, p) == IF TopLevel(p) THEN TRUE ELSE tree[h][SubSeq(p, 1, Len(p) - 1)] = Dir
Unchanged == <<up, lease, role, myterm, own, copy, bb, conflicts, writers, later, disputed, discarded, failures, changes, lies>>

Write(h, p, v) ==
    /\ edits < MaxEdits /\ up[h] /\ ParentOk(h, p) /\ tree[h][p] # v
    /\ tree' = [tree EXCEPT ![h] = Cleared(tree[h], p, v)]
    /\ written' = written \cup {<<h, p, v>>}
    /\ superseded' = superseded \cup FilesIn(tree[h], p)
    /\ edits' = edits + 1
    /\ UNCHANGED Unchanged

MkDir(h, p) ==
    /\ edits < MaxEdits /\ up[h] /\ ParentOk(h, p) /\ tree[h][p] # Dir
    /\ tree' = [tree EXCEPT ![h] = Cleared(tree[h], p, Dir)]
    /\ superseded' = superseded \cup FilesIn(tree[h], p)
    /\ edits' = edits + 1
    /\ UNCHANGED <<written>> /\ UNCHANGED Unchanged

Remove(h, p) ==
    /\ edits < MaxEdits /\ up[h] /\ tree[h][p] # NoFile
    /\ tree' = [tree EXCEPT ![h] = Cleared(tree[h], p, NoFile)]
    /\ superseded' = superseded \cup FilesIn(tree[h], p)
    /\ edits' = edits + 1
    /\ UNCHANGED <<written>> /\ UNCHANGED Unchanged

-----------------------------------------------------------------------------
(* Failure *)

Crash(h) ==
    /\ failures < MaxFailures /\ up[h]
    /\ up' = [up EXCEPT ![h] = FALSE]
    /\ failures' = failures + 1
    /\ UNCHANGED <<tree, lease, role, myterm, own, copy, bb, conflicts, writers, later, disputed, written, superseded, discarded, edits, changes, lies>>

\* Back as a follower — unless the host's own lease still names it, in
\* which case nobody took over and it resumes.
Recover(h) ==
    /\ ~up[h]
    /\ up' = [up EXCEPT ![h] = TRUE]
    /\ role' = [role EXCEPT ![h] = IF lease[h].leader = h THEN "leading" ELSE "following"]
    /\ myterm' = [myterm EXCEPT ![h] = lease[h].term]
    /\ UNCHANGED <<tree, lease, own, copy, bb, conflicts, writers, later, disputed, written, superseded, discarded, edits, failures, changes, lies>>

-----------------------------------------------------------------------------
(* Leadership *)

Admits(current, presented) ==
    presented.term > current.term \/ (presented.term = current.term /\ presented.leader = current.leader)

\* Adoption, on host h, of its copy of the (alpha, b) session's ancestor:
\* when the copy holds history, and was written after the host's own store
\* or the host has none. What is taken up is the copy less every path
\* where it disagrees with the host's tree, and everything beneath: those
\* are forgotten, and the next cycle reconciles them as new.
Adopts(h, b) == copy[h][b].gen > 0 /\ (own[h][b].gen = 0 \/ later[h][b])
SetAside(h, b) == {p \in Paths : \E q \in Prefixes(p) : copy[h][b].tree[q] # tree[h][q]}
Taken(h, b) ==
    [tree |-> [p \in Paths |-> IF p \in SetAside(h, b) THEN NoFile ELSE copy[h][b].tree[p]],
     gen |-> copy[h][b].gen]
Adopted(h, b) == IF Adopts(h, b) THEN Taken(h, b) ELSE own[h][b]
Disputed(h, b) ==
    IF Adopts(h, b)
    THEN {<<h, p, tree[h][p]>> : p \in {p \in SetAside(h, b) : tree[h][p] \in Values}}
    ELSE {}

\* A beta takes the lead: its lease looks stale (the leader it knows of is
\* down, or, if Flaky, whenever), every beta ahead of it in the order is
\* down or is that leader, and it is not leading already.
Takeover(b) ==
    /\ changes < MaxChanges
    /\ up[b] /\ role[b] = "following"
    /\ Flaky \/ ~up[lease[b].leader]
    /\ \A e \in Earlier(b) : ~up[e] \/ e = lease[b].leader
    /\ LET t == lease[b].term + 1 IN
       /\ myterm' = [myterm EXCEPT ![b] = t]
       /\ lease' = [lease EXCEPT ![b] = [leader |-> b, term |-> t]]
    /\ role' = [role EXCEPT ![b] = "leading"]
    /\ own' = [own EXCEPT ![b][b] = Adopted(b, b)]
    /\ later' = [later EXCEPT ![b][b] = IF Adopts(b, b) THEN FALSE ELSE @]
    /\ disputed' = disputed \cup Disputed(b, b)
    /\ bb' = [bb EXCEPT ![b] = [c \in Betas |-> Empty]]
    /\ changes' = changes + 1
    /\ UNCHANGED <<tree, up, copy, conflicts, writers, written, superseded, discarded, edits, failures, lies>>

\* The alpha takes the lead back from the beta it follows, once their
\* session has settled and the alpha's copy of its ancestor is level with
\* the leader's.
Handoff ==
    /\ changes < MaxChanges
    /\ up["alpha"] /\ role["alpha"] = "following"
    /\ LET l == lease["alpha"].leader IN
       /\ l \in Betas /\ up[l] /\ role[l] = "leading"
       /\ tree["alpha"] = tree[l]
       /\ copy["alpha"][l] = own[l][l]
       /\ LET t == lease["alpha"].term + 1 IN
          /\ myterm' = [myterm EXCEPT !["alpha"] = t]
          /\ lease' = [lease EXCEPT !["alpha"] = [leader |-> "alpha", term |-> t]]
    /\ role' = [role EXCEPT !["alpha"] = "leading"]
    /\ own' = [own EXCEPT !["alpha"] = [b \in Betas |-> Adopted("alpha", b)]]
    /\ later' = [later EXCEPT !["alpha"] = [b \in Betas |-> IF Adopts("alpha", b) THEN FALSE ELSE @[b]]]
    /\ disputed' = disputed \cup UNION {Disputed("alpha", b) : b \in Betas}
    /\ changes' = changes + 1
    /\ UNCHANGED <<tree, up, copy, bb, conflicts, writers, written, superseded, discarded, edits, failures, lies>>

-----------------------------------------------------------------------------
(* Sessions *)

\* In the alpha mode, the configured alpha's version wins wherever the
\* alpha is involved, whoever leads; between two betas, the leader's does.
AlphaSide(c, h) == IF h = "alpha" THEN "alpha" ELSE c
BetaSide(c, h) == IF h = "alpha" THEN c ELSE h

\* The ancestor a controller c uses for its session with host h.
Anc(c, h) ==
    IF c = "alpha" THEN own["alpha"][h].tree
    ELSE IF h = "alpha" THEN own[c][c].tree
    ELSE bb[c][h]

\* A cycle of leader c's session with host h: present the lease, and if
\* it is admitted, reconcile. A refused lease steps the controller down.
Cycle(c, h) ==
    /\ role[c] = "leading" /\ up[c] /\ up[h] /\ h # c
    /\ LET presented == [leader |-> c, term |-> myterm[c]] IN
       IF Admits(lease[h], presented) THEN
         LET a == Anc(c, h)
             x == tree[AlphaSide(c, h)]
             y == tree[BetaSide(c, h)]
             O == [p \in Paths |-> Outcome(a, x, y, p)]
             x2 == [p \in Paths |-> O[p].alpha]
             y2 == [p \in Paths |-> O[p].beta]
             a2 == [p \in Paths |-> O[p].anc]
             bumped(s) == IF s.tree = a2 THEN s ELSE [tree |-> a2, gen |-> s.gen + 1]
         IN
         /\ lease' = [lease EXCEPT ![h] = presented]
         /\ writers' = [writers EXCEPT ![h] = @ \cup {presented}]
         /\ tree' = [tree EXCEPT ![AlphaSide(c, h)] = x2, ![BetaSide(c, h)] = y2]
         \* A store written moves its copy to before it.
         /\ IF c = "alpha" THEN /\ own' = [own EXCEPT !["alpha"][h] = bumped(@)]
                                /\ later' = [later EXCEPT !["alpha"][h] = IF a2 = own["alpha"][h].tree THEN @ ELSE FALSE]
                                /\ UNCHANGED bb
            ELSE IF h = "alpha" THEN /\ own' = [own EXCEPT ![c][c] = bumped(@)]
                                     /\ later' = [later EXCEPT ![c][c] = IF a2 = own[c][c].tree THEN @ ELSE FALSE]
                                     /\ UNCHANGED bb
            ELSE bb' = [bb EXCEPT ![c][h] = a2] /\ UNCHANGED <<own, later>>
         /\ conflicts' = [conflicts EXCEPT ![c][h] = {p \in Paths : O[p].conflict}]
         /\ discarded' = discarded \cup Lost(BetaSide(c, h), y, y2, x2, a) \cup Lost(AlphaSide(c, h), x, x2, y2, a)
         /\ UNCHANGED <<up, role, myterm, copy, disputed, written, superseded, edits, failures, changes, lies>>
       ELSE
         /\ role' = [role EXCEPT ![c] = "following"]
         /\ UNCHANGED <<tree, up, lease, myterm, own, copy, bb, conflicts, writers, later, disputed, written, superseded, discarded, edits, failures, changes, lies>>

\* The leader of an (alpha, b) session pushes its ancestor to the other
\* side, which replaces its replica with it — written after the host's own
\* store, now. Any lag, since this is its own step.
Replicate(c, h) ==
    /\ role[c] = "leading" /\ up[c] /\ up[h] /\ h # c
    /\ lease[h].leader = c
    /\ "alpha" \in {c, h}
    /\ LET b == BetaSide(c, h) IN
       /\ copy[h][b] # own[c][b]
       /\ copy' = [copy EXCEPT ![h][b] = own[c][b]]
       /\ later' = [later EXCEPT ![h][b] = TRUE]
    /\ UNCHANGED <<tree, up, lease, role, myterm, own, bb, conflicts, writers, disputed, written, superseded, discarded, edits, failures, changes, lies>>

\* With Lies: the partner of an (alpha, b) session writes a copy that is
\* no record of anything — a buggy leader, or a dishonest one. Only the
\* partner, since a host takes up a copy from no one else.
Corrupt(h, b, t) ==
    /\ Lies /\ lies < MaxLies
    /\ up[h] /\ h \in {"alpha", b}
    /\ Formed(t)
    /\ copy' = [copy EXCEPT ![h][b] = [tree |-> t, gen |-> @.gen + 1]]
    /\ later' = [later EXCEPT ![h][b] = TRUE]
    /\ lies' = lies + 1
    /\ UNCHANGED <<tree, up, lease, role, myterm, own, bb, conflicts, writers, disputed, written, superseded, discarded, edits, failures, changes>>

Next ==
    \/ \E h \in Hosts, p \in Paths, v \in Values : Write(h, p, v)
    \/ \E h \in Hosts, p \in Paths : MkDir(h, p) \/ Remove(h, p)
    \/ \E h \in Hosts : Crash(h) \/ Recover(h)
    \/ \E b \in Betas : Takeover(b)
    \/ Handoff
    \/ \E c \in Hosts, h \in Hosts : Cycle(c, h) \/ Replicate(c, h)
    \/ \E h \in Hosts, b \in Betas, t \in [Paths -> Content] : Corrupt(h, b, t)

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ \A c \in Hosts, h \in Hosts : WF_vars(Cycle(c, h)) /\ WF_vars(Replicate(c, h))
    /\ \A h \in Hosts : WF_vars(Recover(h))
    /\ \A b \in Betas : WF_vars(Takeover(b))
    /\ WF_vars(Handoff)

-----------------------------------------------------------------------------
(* Properties *)

TypeOK ==
    /\ tree \in [Hosts -> [Paths -> Content]]
    /\ role \in [Hosts -> {"leading", "following"}]
    /\ edits \in 0..MaxEdits /\ failures \in 0..MaxFailures /\ changes \in 0..MaxChanges
    /\ lies \in 0..MaxLies /\ later \in [Hosts -> [Betas -> BOOLEAN]]

WellFormed == \A h \in Hosts : Formed(tree[h])

Present(p, v) == \E h \in Hosts : tree[h][p] = v

Accounted ==
    \A w \in written :
        \/ Present(w[2], w[3])
        \/ <<w[2], w[3]>> \in superseded
        \/ \E s \in Hosts : <<s, w[2], w[3]>> \in discarded

NeverDiscards == Mode = "conflict" => discarded = {}

\* The configured alpha's values are never discarded, whoever leads.
AlphaKeeps == \A d \in discarded : d[1] # "alpha"

\* A value a host held where a copy it adopted disagreed is never lost:
\* on some host still, or replaced by a user, or on record as discarded —
\* which in the conflict mode nothing is.
DisputedKept ==
    \A d \in disputed :
        \/ Present(d[2], d[3])
        \/ <<d[2], d[3]>> \in superseded
        \/ \E s \in Hosts : <<s, d[2], d[3]>> \in discarded

\* One writer per host per term.
Fenced ==
    \A h \in Hosts : \A w1, w2 \in writers[h] : w1.term = w2.term => w1.leader = w2.leader

\* The term a host is written at never falls.
TermsRise == [][\A h \in Hosts : lease'[h].term >= lease[h].term]_vars

\* A value a user removed, gone from every host, comes back only by a
\* user writing it again.
UserWrote(p, v) == edits' = edits + 1 /\ \E h \in Hosts : tree'[h][p] = v /\ tree[h][p] # v
NoResurrection ==
    [][\A p \in Paths, v \in Values :
        (<<p, v>> \in superseded /\ ~Present(p, v) /\ Present(p, v)') => UserWrote(p, v)]_vars

Leader == CHOOSE c \in Hosts : role[c] = "leading" /\ up[c]
Stable == \E c \in Hosts : role[c] = "leading" /\ up[c]
InConflict(c, h, p) == \E q \in conflicts[c][h] : p \in Subtree(q)
Level ==
    /\ Stable
    /\ \A h \in Hosts : (up[h] /\ h # Leader) =>
          \A p \in Paths : tree[h][p] = tree[Leader][p] \/ InConflict(Leader, h, p)

Converges == <>[]Level

=============================================================================
