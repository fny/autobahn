
`autobahn status` should feature the folder prominently followed by the project id and then each connection
status


<b>~/.fny</b> fny
  faraz@boite:~/.fny
    status:  inactive (unreachable at last start) (synchonized  1 cycle (10s ago)) error etc
    conflicts: ...
    mode: two-way-resolved
  claude@faraz.vip:~/.fny
    status:  inactive (unreachable at last start)
  ubuntu@fny.voltai.party:~/.fny
    status:  inactive (unreachable at last start)


---


`autobahn up` should return connection statuses alone.
check is for connected
x is for disconnected
grayed with no symbol means the host is disabled


<b>aws</b>
  ✓  boite
  ✓  faraz.vip
  ✓  fny.voltai.party
  ✗  gpu.voltai.party
  <span class="disabled">  soros</span>
<b>fny</b>
  ✓  boite
  ✓  faraz.vip
  ✓  fny.voltai.party
  ✗  gpu.voltai.party

---
