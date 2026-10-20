# Full-page text extraction (layout fallback)

<!-- PAGE 1 -->

 Endurable Transient Inconsistency in Byte-
      Addressable Persistent B+-Tree

Deukyeon Hwang and Wook-Hee Kim, UNIST; Youjip Won, Hanyang University;
                    Beomseok Nam, UNIST
          https://www.usenix.org/conference/fast18/presentation/hwang



        This paper is included in the Proceedings of the
   16th USENIX Conference on File and Storage Technologies.
              February 12–15, 2018 • Oakland, CA, USA
                          ISBN 978-1-931971-42-3



                            Open access to the Proceedings of
                                   the 16th USENIX Conference on
                                               File and Storage Technologies
                                                      is sponsored by USENIX.

<!-- PAGE 2 -->

 Endurable Transient Inconsistency in Byte-Addressable Persistent B+-Tree


    Deukyeon Hwang    Wook-Hee Kim         Youjip Won       Beomseok Nam
        UNIST           UNIST        Hanyang University       UNIST




                Abstract                       and dynamic resizing. Besides, the large CPU cache in
                                                modern processors[11] enables B+-tree to exhibit a good
  With the emergence of byte-addressable persistent
                                                      cache line locality. As a result, B+-tree shows good per-
memory (PM), a cache line, instead of a page, is expected
                                                    formance as an in-memory data structure as well. Thus,
to be the unit of data transfer between volatile and non-
                                                            the cache-conscious variants of B+-tree including CSS-
volatile devices, but the failure-atomicity of write oper-
                                                                 tree [36], CSB-tree [37], and FAST [19] are shown to
ations is guaranteed in the granularity of 8 bytes rather
                                                     perform better than legacy binary counterparts such as
than cache lines. This granularity mismatch problem has
                                                              T-tree [26].
generated interest in redesigning block-based data struc-
                                                           Byte-addressable PM raises new challenges in usingtures such as B+-trees. However, various methods of
                                                        B+-tree because legacy B+-tree operations are imple-modifying B+-trees for PM degrade the efﬁciency of B+-
                                                  mented upon the assumption that block I/O is failuretrees, and attempts have been made to use in-memory
                                                         atomic. In modern computer architectures, the unit ofdata structures for PM.
                                                          atomicity guarantee and the unit of transfer do not coin-  In  this  study, we  develop  Failure-Atomic  ShifT
                                                                cide. The unit of atomicity in memory operations corre-(FAST) and Failure-Atomic In-place Rebalance (FAIR)
                                                     sponds to a word, e.g. 64 bits whereas the unit of transferalgorithms to resolve the granularity mismatch problem.
                                                   between the CPU and memory corresponds to a cacheEvery 8-byte store instruction used in the FAST and
                                                                      line, e.g. 64 Bytes. This granularity mismatch is of noFAIR algorithms transforms a B+-tree into another con-
                                                      concern in current memory since  it is volatile. Whensistent state or a transient inconsistent state that read op-
                                                            the memory is non-volatile, unexpected system failureserations can tolerate. By making read operations tolerate
                                          may cause the result of incomplete cache ﬂush opera-transient inconsistency, we can avoid expensive copy-on-
                                                               tions to be externally visible after the system recovers.write, logging, and even the necessity of read latches so
                                               To make the granularity mismatch problem even worse,that read transactions can be non-blocking. Our experi-
                                                modern processors often make the most use of memorymental results show that legacy B+-trees with FAST and
                                                    bandwidth by changing the order of memory operations.FAIR schemes outperform the state-of-the-art persistent
                                                          Besides, recently proposed memory persistency modelsindexing structures by a large margin.
                                                     such as epoch persistency [9] even allow cache lines to
                                                    be written back out of order. To prevent the reordering of
1  Introduction                             memory write operations and to ensure that the written
Recent advances in byte-addressable persistent mem-    data are ﬂushed to PM without compromising the consis-
ories (PM), such as phase change memory[48], spin-    tency of the tree structure, B+-trees for persistent mem-
transfer torque MRAM[17], and 3D Xpoint[2] have    ory use explicit memory fence operations and cache ﬂush
opened up new opportunities for the applications to war-    operations to control the order of memory writes [5, 42].
rant durability or persistency without relying on legacy     The recently proposed B+-tree variants such as NV-
heavy-duty interfaces offered by the ﬁlesystem and/or by     tree [49], FP-tree [34], and wB+-tree [5] have pointed
the block device [21, 24].                                 out and addressed two problems of byte-addressable per-
  In legacy block devices, B+-tree has been one of the     sistent B+-trees. The ﬁrst problem is that a large number
most popular data structures. The primary advantage of    of fencing and cache ﬂush operations are needed to main-
a B+-tree is its efﬁcient data access performance due to     tain the sorted order of keys. And, the other problem is
its high degree of node fan-out, a balanced tree height,     that the logging demanded by tree rebalancing operations




USENIX Association                            16th USENIX Conference on File and Storage Technologies  187

<!-- PAGE 3 -->

is expensive in the PM environment.                 how to enable non-blocking read transactions. Section 6
  To resolve the ﬁrst problem, previous studies have pro-    evaluates the performance of the proposed persistent B+-
posed a way to update tree nodes in an append-only     tree and Section 7 discusses other research efforts. In
manner and introduced additional metadata to the B+-    Section 8 we conclude this paper.
tree structures. With these augmentations, the size of up-
dated memory region in a B+-tree node remains minimal.                                         2  B+-tree for Persistent Memory
However, the additional metadata for indirect access to
the keys, and the unordered entries affect the cache lo-    2.1  Challenge: clﬂush and mfence
cality and increase the number of accessed cache lines,   A popular solution to guarantee transaction consistency
which can degrade search performance.                  and data integrity is the copy-on-write technique, which
  To resolve the second problem of persistent B+-trees    updates the block in an out-of-place manner. The copy-
- logging demanded by  tree rebalancing operations,    on-write update for block-based data structures can be
NVTree [49] and FP-tree [34] employed selective per-    overly expensive because it duplicates entire blocks in-
sistence that keeps leaf nodes in the PM but internal    cluding the unmodiﬁed portion of the block.
nodes in volatile DRAM. Although the selective persis-     The main challenges in employing the in-place update
tence makes logging unnecessary, it requires the recon-    scheme in B+-trees is that we store multiple key-pointer
struction of tree structures on system failures and makes     entries in one node and that the entries must be stored in
the instant recovery impossible.                          a sorted order. Inserting a new key-pointer entry in the
  In this study, we revisit B+-trees and propose a novel    middle of an array will shift on average half the entries.
approach to tolerating transient inconsistency, i.e., par-    In the recent literature [42, 5, 49], this shift operation has
tially updated inconsistent tree status. This approach    been pointed out as the main reason why B+-trees call
guarantees the failure atomicity of B+-tree operations   many cache line ﬂush and memory fence instructions. If
without signiﬁcant fencing or cache ﬂush overhead. If   we do not guard the memory write operations with mem-
transactions are made to tolerate the transient inconsis-    ory fence operations, the memory write operations can
tency, we do not need to maintain a consistent backup    be reordered in modern processors. And, if we do not
copy and expensive logging can be avoided. The key con-    ﬂush the cache lines properly, B+-tree nodes can be up-
tributions of this work are as follows.                      dated partially in PM because some cache lines will stay
                                                               in CPU caches. To avoid such a large number of cache
• We develop Failure Atomic Shift (FAST) and Failure-
                                                                 line ﬂushes and memory fence operations, append-only
  Atomic In-place Rebalance (FAIR) algorithms that
                                                       update strategy can be employed [49, 34, 5]. However,
  transform a B+-tree through endurable transient in-
                                                            the append-only update strategy improves the write per-
   consistent states. The endurable transient inconsistent
                                                    formance at the cost of a higher read overhead because
   state is the state in which read transactions can detect
                                                                                        it requires additional metadata or all unsorted keys in a
  incomplete previous transactions and ignore inconsis-
                                                                 tree node to be read [6].
  tency without hurting the correctness of query results.
  Given that all pointers in B+-tree nodes are unique,
                                                 2.2  Reordering Memory Accesses  the existence of duplicate pointers enables the system
   to identify the state of the transaction at the time of   The reordering of memory write operations helps bet-
  crash and to recover the B+-tree to the consistent state     ter utilize the memory bandwidth [38]. In the last few
  without logging.                                       decades, the performance of modern processors has im-
                                                     proved at a much faster rate than that of memory [10]. In
• We make read transactions non-blocking.  If every
                                                          order to resolve the performance gap between CPU speed
   store instruction transforms a B+-tree index to an-
                                                   and memory access time, memory is divided into multi-
  other state that guarantees correct search results, read
                                                            ple cache banks so that the cache lines in the banks can
   transactions do not have to wait until concurrent write
                                                    be accessed in parallel. As a result, memory operations
   transactions ﬁnish changing the B+-tree nodes.
                                                     can be executed out of order.
  In the sense that the proposed scheme warrants con-     To design a failure-atomic data structure for PM, we
sistency via properly ordering the individual operations    need to consider both volatile memory order and persist
and the inconsistency is handled by read operations, they     order. Let us examine volatile memory order ﬁrst. Mem-
share much of the same idea as the soft-update tech-    ory reordering behaviors vary across architectures and
nique [12, 31] and NoFS [7].                       memory ordering models. Some architectures such as
  The rest of the paper is organized as follows: In Sec-  ARM allow store instructions to be reordered with each
tion 2, we present the challenges in designing a B+-tree     other[8], while other architectures such as x86 prevent
index for PM. In Section 3 and Section 4, we propose     stores-after-stores from being reordered [40, 18], that is,
the FAST and FAIR algorithms. In Section 5, we discuss     total store ordering is guaranteed. However, most archi-




188  16th USENIX Conference on File and Storage Technologies                      USENIX Association

<!-- PAGE 4 -->

tectures arbitrarily reorder stores-after-loads, loads-after-
stores, and loads-after-loads unless dependencies exist in
them. The Alpha is known to be the only processor that
reorders dependent loads [30]. Given that the Alpha pro-
cessor has deprecated since 2003, we consider that all
modern processors do not reorder dependent loads.
  Memory persistency [35] is a framework that provides
an interface for enforcing persist ordering constraints on
PM writes. Persist order in the strict persistency model
matches the memory order speciﬁed in the memory con-
sistency model. However, the memory persistency model
may allow the persist order to deviate from the volatile       Figure 1: FAST Inserting (25, Ptr) into B-tree node
order under the relaxed persistency model [9, 35]. To
simplify our discussion, we ﬁrst present algorithms as-
                                                    be ﬂushed to PM. In FAST, such a premature cache linesuming the strict persistency model. Later, in Section 7,
                                                  ﬂush does not affect the consistency of a sorted array.we will discuss the relaxed persistency model.
                                               The only condition that FAST requires is that the dirty
                                                      cache lines must be ﬂushed in order.
3  Failure-Atomic ShifT (FAST)                  Suppose that an array element is shifted from one
3.1  Shift and Memory Ordering                cache line to another. Since there exists a dependency
                                                   between the load and the store instructions, they will notIn most architectures, the order of memory access oper-
                                                    be reordered and we do not need to explicitly call a mem-ations is not changed arbitrarily if stores and loads have
                                                       ory fence instruction. However, since cache line ﬂush in-dependencies. Based on this observation, we propose the
                                                               structions can be reordered with store instructions, weFailure-Atomic ShifT (FAST) scheme for B+-tree nodes.
                                                                    call a memory fence instruction per cache line ﬂush.FAST frees the shift operation from explicitly calling a
memory fence and a cache line ﬂush instruction with-
                                                 3.2  Endurable Inconsistency during Shiftout compromising the ordering requirement. The idea of
FAST is simple and straightforward. Let us ﬁrst examine   The shift operation is not failure-atomic by itself because
the case where total store ordering (TSO) is guaranteed.     shifting an element can make it appear twice in a row.
  The process of shifting array elements is a sequence    In case of a system failure, such transient duplicate el-
of load and store instructions that are all dependent and    ements can persist as shown in Figure 1, which seems
must be called in a cascading manner. To insert an el-     at ﬁrst glance unacceptably inconsistent. To resolve this
ement in a sorted array, we visit the array elements in     issue, we use one property of B+-tree nodes; B+-tree
reverse order, that is, from the rightmost element to the    nodes do not allow duplicate pointers. Therefore, a key
left ones. Until we ﬁnd an element e[i] that is smaller     in between duplicate pointers found during tree traversals
than the element we insert, we shift the elements to the    can be ignored by transactions and considered as a toler-
right: e[j + 1] ←e[j], j = N,...,i + 1. The dependency    able transient inconsistent state. After ﬁnding a key of in-
between the consecutive move operations, e[i+1] ←e[i]     terest in a sorted array, we check if its left and right child
and e[i] ←e[i −1], prohibits the CPU from performing    pointers have the same addresses. If so, transactions ig-
Out-Of-Order writes[38] and guarantees that the records    nore the key and continue to read the next key. This mod-
are moved while satisfying the preﬁx constraint [45],    iﬁcation in traversal algorithm requires only one more
i.e., if e[i] ←e[i −1] is successful, all preceding moves,    compare instruction per each tree node visit, which in-
e[j] ←e[j −1], j = i+1,...,N have been successful.       curs negligible overhead to transactions.
   If the array size spans across multiple cache lines, it
is uncertain which cache line will be ﬂushed ﬁrst if we    3.3  Insertion with FAST for TSO
do not explicitly call clflush. Therefore, our proposed    In B+-tree nodes, the same number of keys and pointers
FAST algorithm calls a cache line ﬂush instruction when     (or keys and values) need to be shifted in tandem. We
we shift an array element from one cache line to its adja-     store keys and pointers as an array of structure in B+-
cent cache line. Since FAST calls cache line ﬂushes only     tree nodes so that the corresponding keys and pointers
when it crosses the boundary of cache lines, it calls cache    are always ﬂushed together.
line ﬂush instructions only as many times as the number     The insertion algorithm is shown in Algorithm 1. Con-
of dirty cache lines in a B+-tree node.                        sider the example shown in Figure 1. We insert a key-
  Even if we do not call a cache line ﬂush instruction,    value pair (25,Ptr) into the B+-tree node shown in (1)
a dirty cache line can be evicted from CPU caches and    of Figure 1. To make a space for 25, the two rightmost




USENIX Association                            16th USENIX Conference on File and Storage Technologies  189

<!-- PAGE 5 -->

Algorithm 1                                           a system crashes at this point, the key 40 between the re-
FAST insert(node,key, ptr)                             dundant pointers ([P4,40,P4∗]) will be ignored. Next,
 1: node.lock.acquire()                                      the key 30 can be shifted to the right by overwriting
 2:  if (sibling ←node.sibling ptr)̸ = NULL then               the key 40 in step (6). Next, in step (7), we shift P3
 3:     if sibling.records[0].key < key then                       to the right to invalidate the old key 30 ([P3,30,P3∗])
 4:     – previous write thread has split this node            and make space for the key 25, that we insert. In step
 5:       node.lock.release()                                       (8), we store 25 in the third slot. However, the key 25
 6:     FAST insert(sibling,key, ptr);                                                                         is not valid because  it is in between the same point-
 7:      return                                                               ers ([P3,25,P3∗]). Finally, in step (9), we overwrite P3∗
 8:   end if
                                                       with the new pointer Ptr, which will validate the new key
 9: end if
                                                           25. In FAST, writing a new pointer behaves as a commit10:  if node.cnt < node capacity then
                                               mark of an insertion.11:     if node.search dir ﬂag is odd then
12:     – if this node was updated by a delete thread, we          Unlike pointers, the size of keys can vary. If a key size
13:     – increase this ﬂag to make it even so that                 is greater than 8 bytes, it cannot be written atomically.
14:     – lock-free search scans from left to right             However, our proposed FAST insertion makes changes
15:      node.search dir ﬂag++;                                to a key only if it is located between the same pointers.
16:   end if                                               Therefore, even if a key size is larger than 8 bytes and
17:    for i ←node.cnt −1;i ≥0;i−−do                   even if it cannot be updated atomically, read operations
18:         if node.records[i].key > key then                                                         ignore such partially written keys and guarantee correct
19:         node.records[i+1].ptr ←node.records[i].ptr;
                                                         search results.
20:     m fence IF NOT TSO();
                                                             In this sense, every single 8-byte write operation in21:         node.records[i+1].key ←node.records[i].key;
                                                            the FAST insertion algorithm is failure-atomic and crash22:     m fence IF NOT TSO();
23:             if &(node.records[i+1]) is at cacheline boundary     consistent because partially written tree nodes are always
           then                                             usable. Hence, even if a system crashes during the up-
24:              cl flush with mfence(&node.records[i+1]);      date, FAST guarantees recoverability as long as we ﬂush
25:       end if                                               dirty cache lines in an order.
26:       else
27:         node.records[i+1].ptr ←node.records[i].ptr;      3.4  FAST for Non-TSO Architectures
28:     m fence IF NOT TSO();
                                ARM processors can reorder store instructions if they do29:         node.records[i+1].key ←key;
                                                         not have a dependency. I.e., if a store instruction updates30:     m fence IF NOT TSO();
31:         node.records[i+1].ptr ←ptr;                    a key and another store instruction updates a pointer, they
32:           cl flush with mfence(&node.records[i+1]);       can be reordered.
33:     end if                                                         First, consider the case where keys are no larger than
34:   end for                                      8 bytes. Then, keys and pointers can be shifted indepen-
35:    node.lock.release()                                   dently because no key or pointer in either array can be in
36: else                                                a partially updated status. Suppose that the ith and i+1th
37:    node.lock.release()                                                      keys are the same and the jth and j+1th pointers are the
38:    FAIRsplit(node,key, ptr);                                               same (i̸ = j). We can simply ignore one of the redundant
39: end if
                                                       elements in both arrays, easily sort out which pointer is
                                                               for which key’s child node, and reconstruct the correct
                                                               logical view of the B+-tree node.
keys, 30 and 40, their pointers P4 and P5, and the sen-                                                        Second, consider the case where the keys are larger
tinel pointer Null must be shifted to the right.                                                         than 8 bytes. One easy option is to call a memory fence
   First, we shift the sentinel pointer Null to the right as     instruction for each shift operation. Although this op-
shown in (2). Next, we shift the right child pointer P5 of     tion calls a large number of memory fence instructions, it
the last key 40 to the right, and then we shift key 40 to the     calls cache line ﬂush instructions only as many times as
right, as shown in (3) and (4). In step (3) and (4), we have    the number of dirty cache lines as in TSO architectures.
a garbage key and a duplicate key 40 as the last key re-    Although memory fence overhead is not negligible, it is
spectively. Such inconsistency can be tolerated by mak-   much smaller than cache line ﬂush overhead. Alterna-
ing other transactions ignore the key between the same     tively, we can make B+-tree store the large keys in a sep-
pointers (P5 and P5∗in the example).                        arate memory heap and store pointers to the keys in B+-
  In step (5), we shift P4 by overwriting P5. This atomic     tree nodes. This option allows us to avoid a large number
write operation invalidates the old key 40 ([P4,40,P4∗])    of memory fence instructions. However, through experi-
and validates the shifted key 40 ([P4∗,40,P5∗]). Even if    ments, we found the indirect access and the poor cache




190  16th USENIX Conference on File and Storage Technologies                      USENIX Association

<!-- PAGE 6 -->

                                          Figure 2: FAIR Node Split


locality signiﬁcantly degrade the search performance.      Algorithm 2
                                             FAIR split(node,key, ptr)
3.5  Deletion with FAST                                       1: node.lock.acquire()
Deletion can be performed in a similar manner but       2:  if (sibling ←node.sibling ptr)̸ = NULL then
in reverse order of insertion. Consider the deletion of       3:     if sibling.records[0].key < key then
(25,Ptr) from the tree node illustrated in (9) of Figure 1.       4:       node.lock.release()
                                                                                5:     FAST insert(sibling,key, ptr);First, we overwrite Ptr with P3 to invalidate the key 25.
                                                                                6:      return
Note that this atomic write operation behaves as the com-
                                                                                7:   end if
mit mark of the deletion. If a system crashes and a sub-                                                                                8: end if
sequent query ﬁnds the B+-tree node as shown in (8), the       9: sibling ←nv malloc(sizeo f(node));
key 25 ([P3,25,P3∗]) will be considered as an invalid     10: median ←node capacity/2
key because it is in between duplicate pointers.                 11: for i ←median;i < node capacity;i++ do
  In the subsequent steps shown in (7)∼(3), the pointer     12:   FAST insert without lock(sibling,
array will have adjacent duplicate pointers that invali-            node.records[i].key,node.records[i].ptr);
date one of the keys while we shift them to the left. Fi-     13: end for
nally, in step (2), we shift the sentinel pointer Null to the     14: sibling.sibling ptr ←node.sibling ptr;
                                                                        15: cl flush with m fence(sibling);left, which completes the deletion process. Similar to the
                                                                        16: node.sibling ptr ←sibling;
insertion, we ﬂush dirty cache lines only when we are
                                                                        17: cl flush with m fence(&node.sibling ptr);
about to make the next cache line dirty.
                                                                        18: node.records[median].ptr ←NULL;
                                                                        19: cl flush with m fence(&node.records[median]);
4  Failure-Atomic In-place Rebalancing           20: – split is done. now insert (key,ptr)
4.1  FAIR: Node Split                                      21:  if key < node.records[median].key then
                                                                        22:   FAST insert without lock(node,key, ptr)
In both legacy disk-based B-trees and recently proposed
                                                                        23: else
B+-tree variants for PM [49, 34, 5], logging or journaling
                                                                        24:   FAST insert without lock(sibling,key, ptr)
has been used when splitting or merging multiple tree                                                                        25: end if
nodes. If a tree node splits, (1) we create a sibling node,     26: node.lock.release()
(2) move half of the entries from the overfull node to     27: – update the parent node by traversing from the root.
the new sibling node, and (3) insert a pointer to the new     28: FAST internal node insert(root,node.level+
sibling node and its smallest key to the parent node. Since         1,sibling.records[0].key,node.sibling ptr);
these three steps must be performed atomically, logging
can be used. However, logging duplicates dirty pages. It
not only increases the write trafﬁc but also blocks the
                                                 them out so that we do not have to maintain an additionalconcurrent access to tree nodes.
                                                  copy of data for crash recovery and consistency is not  We avoid the expensive logging by leveraging the
                                                 compromised in the event of system failure.FAST in-place update scheme and the Failure-Atomic In-
place Rebalance (FAIR) node split algorithm described        First, suppose there is only one node in the tree as
in Algorithm 2. In our B+-tree design, we store a sibling   shown in (1). If we insert a new key 50, the leaf node
pointer not just for leaf nodes but also for internal nodes   A will split. In (2), we create a sibling node B using
as in the B-link tree [25].                                a PM heap manager [33], copy half of the entries, call
  Figure 2 illustrates each step of the FAIR node split    cache line ﬂushes for the new node, and make the sibling
algorithm with an example. We examine every individual    pointer of node A point to the sibling node B. The sibling
store operations in splitting a tree node and carefully lay    pointer must be written before we delete the migrated en-




USENIX Association                            16th USENIX Conference on File and Storage Technologies  191

<!-- PAGE 7 -->

tries in the overfull node A. It looks as if the consistency     sibling node to the underutilized node using FAST, and
of the tree nodes is violated because we have duplicate     insert the new smallest key of the right sibling node to its
entries in nodes A and B; however, the right sibling node    parent node. This node merge algorithm is similar to the
B will not be used until we delete duplicate entries from     split algorithm, but it is performed in the reverse order of
A because the smallest key in the right sibling node (40)    the split algorithm. Thus, a working example of the node
is smaller than the largest key in the overfull node (60).     merge algorithm can be illustrated by reversing the order
  Suppose a system crashes in state (2). If a query that    of the steps shown in Figure 2.
searches for key 35 is submitted, it will access node A
following the search path from the root node because the   5  Lock-Free Search
sibling node B has not been added to the parent node                                                    With the growing prevalence of many-core systems,
yet. Since 35 is smaller than 40 (the one in node A), the                                                         concurrent data structures are becoming increasingly im-
search will stop without accessing node B. If the search                                                               portant. In the recently proposed memory driven comput-
key is 65, the query will follow the sibling pointer, ac-    ing environment that consists of a large number of pro-
cess the sibling node B, and compare to see if the ﬁrst                                                           cessors accessing a persistent memory pool [1], concur-
key of the sibling node B is greater than the largest key                                                               rent accesses to shared data structures including B+-tree
in the node A. If it is, the query will keep checking the                                                        indexes must be managed efﬁciently in a thread-safe way.
next key until it ﬁnds a matching key or a larger key                                              As described earlier, the sequences of 8 byte store op-
than the search key. However, in this example, the search                                                              erations in FAST and FAIR algorithms guarantee that
will stop after it ﬁnds that the ﬁrst key 40 in the sibling                                                 no read thread will ever access inconsistent tree nodes
node B is smaller than the largest key 60 in the node A.                                                     even if a write thread fails while making changes to the
The basic assumption that makes this redundancy and in-                                                        B+-tree nodes. In other words, even if a read transac-
consistency tolerable is that the right sibling nodes al-                                                               tion accesses a B+-tree partially updated by a suspended
ways have larger keys than the left sibling nodes. There-                                                           write thread, it is guaranteed that the read transaction will
fore, the keys and pointers in the new right sibling node                                                             return the correct results. On the contrary, read transac-
([P4∗,40∗,P6∗,60∗,Null]) will not be accessed until we                                                               tions can be suspended and a write transaction can make
delete them in the overfull node A. We delete migrated                                                     changes to the B+-tree that the read transactions were ac-
keys and pointers from the overfull node by atomically                                                             cessing. When the read transactions wake up, they need
setting the pointer of the median to NULL. This will                                                               to detect and tolerate the updates made by the write trans-
change the tree status to (3). If we search the key 40 after                                                               action. In our implementation, tree structure modiﬁca-
deleting the migrated entries, the query will ﬁnd that the                                                                   tions, such as page splits or merges, can be handled by
largest key in node A is smaller than the search key, and                                                            the concurrency protocol of the B-link tree [25]. How-
follow the sibling pointer to ﬁnd the key 40 in node B.                                                                ever, the B-link tree has to acquire an exclusive lock to
  Figure 2 (5)∼(7) illustrate how we can endure tran-                                                       update a single tree node atomically. However, leverag-
sient inconsistency when we update the parent node. In                                                         ing the FAST algorithm, we can design a non-blocking
step (5), node A and its sibling node B can be considered                                                             lock-free search algorithm to allow concurrent accesses
as a single virtual node. In the parent node R, we add a                                                               to the same tree node as described below.
key-pointer pair for node B, i.e., (40,C4) in the example.                                                 To enable a lock-free search, all queries must access a
Since the parent node has keys greater than the key 40                                                                 tree node in the same direction. That is, while a write
that we add, we shift them using FAST as shown in (5)                                                           thread is shifting keys and pointers to the right, read
and (6). After we create a space in the parent node by                                                           threads must access the node from left to right. If a write
duplicating the pointer C2, we ﬁnally store (40,C4) as                                                           thread is deleting an entry using left shifts, read threads
shown in (7).                                                  must access the node from right to left. Suppose the fol-
                                                     lowing example. A query accesses the tree node shown
4.2  FAIR: Node Merge                               in Figure 1(1) to search for key 22. After the query reads
In B+-tree, underutilized nodes are merged with sibling    the ﬁrst two keys - 10 and 20, it is then suspended be-
nodes. If a deletion causes a node to be underutilized,    fore accessing the next key 30. While the query thread is
we delete the underutilized node from its parent node so    suspended, a write thread inserts 25 and shifts keys and
that the underutilized node and its left sibling node be-    pointers to the right. When the suspended query thread
come virtually a single node. Once we detach the under-    wakes up later, the tree node may be different from what
utilized node from the parent node, we check whether       it was before. If the tree node is in one of the states
the underutilized node and its left sibling node can be    (1)∼(6), or (9), the read thread will follow the child
merged. If the left sibling node does not have enough en-    pointer P3 without a problem. If the tree node is in state
tries, we merge them using FAST. If the left sibling node     (7) or (8), the read thread will ﬁnd the left and right child
has enough entries, we shift some entries from the left    pointers are the same. Then, it will ignore the current key




192  16th USENIX Conference on File and Storage Technologies                      USENIX Association

<!-- PAGE 8 -->

Algorithm 3                                           our lock-free search algorithm cannot employ a binary
LockFreeSearch(node,key)                                 search. Although the binary search is known to perform
 1: repeat                                                       faster than the linear search, its performance can be lower
 2:     ret ←NULL;                                       than that of the linear search when the array size is small,
 3:    prev switch ←node.switch;                            as we will show in Section 6.2.
 4:     if prev switch is even then
 5:     – we scan this node from left to right               5.1  Lock-Free Search Consistency Model
 6:       for i ←0;records[i].ptr̸ = NULL;i++ do
                                   A drawback of a lock-free search algorithm is that 7:             if (k ←records[i].key) = key&&records[i].ptr̸ =
                (t ←records[i+1].ptr) then                     a deterministic ordering of transactions cannot be en-
 8:                if (k = records[i].key) then                      forced. Suppose a write transaction inserts two keys - 10
 9:                 ret ←t;                               and 20 into a tree node and another transaction performs
10:               break;                                   a range query and accesses the tree node concurrently.
11:          end if                                The range query may ﬁnd 10 in the tree node but may
12:       end if                                          not ﬁnd 20 if it has not been stored yet. This problem
13:     end for                                       can occur because the lock-free search does not serial-
14:    else                                                               ize the two transactions. Although the lock-free search
15:     – this node was accessed by a delete query
                                                        algorithm helps improve the concurrency level, it is vul-
16:     – we have to scan this node from right to left
                                                          nerable to the well-known phantom reads and dirty reads17:       for i ←node.cnt −1;i >= 0;i−−do
                                                    problems [41].18:        – omitted due to symmetry and lack of space
19:     end for                                              In the database community, various levels of isolation
20:   end if                                           such as serializable mode, non-repeatable read mode,
21: until prev switch = node.switch                        read committed mode, and read uncommitted mode [41]
22:  if ret = NULL && (t ←node.sibling ptr) then           have been studied and used. Considering our lock-free
23:     if t.records[0].key ≤key then                          search algorithm is vulnerable to dirty reads, it operates
24:      return  t;                                             in read uncommitted mode. Although read uncommitted
25:   end if                                   mode can be used for certain types of queries in OLAP
26: end if                                                         systems, the scope of its usability is very limited.
27: return ret;
                                                 To support higher isolation levels, we must resort to
                                                           other concurrency control methods such as key range
                                                              locks, snapshot isolation, or multi-version schemes [41].
and move on to the next key so that it follows the correct                                                   However, these concurrency control methods may im-
child pointer. From this example, we can see that shifting                                                     pose a signiﬁcant overhead. To achieve both a high con-
keys and pointers in the same direction does not hurt the                                                        currency level and a high isolation level, we designed an
correctness of concurrent non-blocking read queries.                                                                alternative method to compromise. That is, we use read
   If a read thread scans an array from left to right while a    locks only for leaf nodes considering the commit oper-
write thread is deleting its element by shifting to the left,     ations only occur in leaf nodes. For internal tree nodes,
there is a possibility that the read thread will miss the    read transactions do not use locks since they are irrele-
shifted entries. But if we shift keys and pointers in the    vant to phantom reads and dirty reads. Since transactions
same direction, the suspended read thread cannot miss    are likely to conﬂict more in internal tree nodes rather
any array element even if it may encounter the same en-    than in leaf nodes, read locks in leaf nodes barely affect
try multiple times. However, this does not hurt the cor-    the concurrency level as we will show in Section 6.7.
rectness of the search results.
  To guide which direction read threads scan a tree node,    5.2  Lazy Recovery for Lock-Free Search
we use a counter ﬂag which increases when insertions    Since the reconstruction of a consistent logical view of
and deletions take turn. That is, the ﬂag is an even num-    a B+-tree is always possible with an inconsistent but
ber if a tree node has been updated by insertions and an    correctable B+-tree, we perform a recovery in a lazy
odd number if the node has been updated by deletions.    manner. We do not let read transactions ﬁx tolerable in-
Search queries determine in which direction it scans the    consistency because read transactions are more latency
node according to the ﬂag, and it double checks whether     sensitive than write transactions. In our design, instead,
the counter ﬂag remains unchanged after the scan. If the   we make only write threads ﬁx tolerable inconsistencies.
ﬂag has changed, the search query must scan the node    Such a lazy recovery approach is acceptable because
once again. A pseudo code of this lock-free search algo-   FAST allows at most one pair of duplicate pointers in
rithm is shown in Algorithm 3.                          each node. Thus, it does not waste a signiﬁcant amount
  Because of the restriction on the search direction,    of memory space, and its performance impact on search




USENIX Association                            16th USENIX Conference on File and Storage Technologies  193

<!-- PAGE 9 -->

is negligible. Besides, the lazy recovery is necessary for a     sible. Thus, strictly speaking, FP-Tree is not a persistent
lock-free search. In lock-free searches, read threads and    index.
a write thread can access the same node. If read threads    WORT [23] is an alternative index for PM based upon
must ﬁx the duplicate entries that a write thread caused,    a radix tree. Unlike B+-tree variants, the radix tree does
read threads will compete for an exclusive write lock.    not require key sorting nor rebalancing tree structures.
Otherwise, read threads have to check whether the node    Since the radix tree structure is deterministic, insertion or
is inconsistent due to a crash or due to an inﬂight insert    deletion of a key requires only a few 8 byte write opera-
or delete, which will introduce signiﬁcant complexities     tions. However, radix trees are sensitive to the key distri-
and latency to reads.                                      bution and often suffers from poor cache utilization due
  To ﬁx inconsistent tree nodes, we delete the garbage     to their deterministic tree structures. Also, radix trees are
key in between duplicate pointers by shifting the array    not as versatile as B+-trees as their range query perfor-
to the left. For a dangling sibling node, we check if the   mance is very poor.
sibling node can be merged with its left node. If not, we      SkipList [16] is a probabilistic indexing structure that
insert the pointer to the sibling node into the parent node.    avoids expensive rebalancing operations. An update of
  In the FAIR scheme, the role of adding a sibling node    the skip list needs pointer updates in multi-level linked
to the parent node is not conﬁned to the query that cre-      lists. But only the lowest level-linked list needs to be
ated the sibling node. Even if a process that split a node    updated in a failure-atomic way. In a Log-Structured
crashes for some reason, a subsequent process that ac-  NVMM System [16], SkipList was used as a volatile ad-
cesses the sibling node via the sibling pointer triggers    dress mapping tree, but SkipList shares the same goal
a parent node update. If multiple write queries visit a    with our B+-tree. That is, both indexing structures do not
tree node via the sibling pointer, only one of them will    need logging, enable lock-free search, and beneﬁt from a
succeed in updating the parent node and the rest of the    high degree of parallelism.
queries will ﬁnd that the parent has already been updated.
                                                 6.1  Experimental Environment
6  Experiments                         We run experiments on a workstation that has two In-
                                                                         tel Xeon Haswell-Ex E7-4809 v3 processors (2.0 GHz,We  evaluate  the performance  of FAST and FAIR1
                                                 16 vCPUs with hyper-threading enabled, and 20 MB L3against the state-of-the-art indexing structures for PM
                                                        cache) that guarantee total store ordering and 64 GB of- wB+-tree with slot+bitmap nodes [5], FP-tree [34],
                                     DDR3 DRAM. We compiled all implementations usingWORT [23], and Skiplist [16].
                                             g++ 4.8.2 with -O3 option.
  wB+-tree [5] is a B+-tree variant that stores all tree
                                       We  use  a DRAM-based PM  latency  emulator  -
nodes in PM. wB+-tree inserts data in an append-only
                                                       Quartz[43] as was done in [44, 3, 20]. Quartz models
manner. To keep the ordering of appended keys, wB+-
                                                           application-perceived PM latency by inserting stall cy-
tree employs a small metadata, called slot-array, which
                                                               cles in each predeﬁned time interval called epoch. In our
manages the index of unsorted keys in a sorted order. In
                                                         experiments, the minimum and maximum epochs are set
order to atomically update the slot-array, wB+-tree uses
                                                               to 5 nsec and 10 nsec respectively. We assume that PM
a separate bitmap to mark the validness of array elements
                                                    bandwidth is the same as that of DRAM since Quartz
and slot-array. Because of these metadata, wB+-tree calls
                                                     does not allow us to emulate both latency and bandwidth
at least four cache line ﬂushes when we insert data into a
                                                                    at the same time.
tree node. Besides this, wB+-tree also requires expensive
logging and a large number of cache line ﬂushes when
                                                 6.2  Linear Search vs. Binary Searchnodes split or merge.
                                                           In the ﬁrst set of experiments shown in Figure 3, we in-  FP-Tree [34] is a variant of a B+-tree that stores leaf
                                                   dex 1 million random key-value pairs of 8 bytes eachnodes in PM but internal nodes in DRAM. It proposes to
                                                   and evaluate the performance of the linear search and thereduce the CPU cache miss ratio via ﬁnger printing and
                                                         binary search with varying size of B+-tree nodes. We as-employs hardware transactional memory to control con-
                                              sume the latency of PM is the same as that of DRAM.current access to internal tree nodes in DRAM. FP-Tree
                                              As we increase the size of the tree nodes, the treeclaims that internal nodes can be reconstructed without
                                                           height decreases in log scale but the number of data to besigniﬁcant overhead. However, the reconstruction of in-
                                                               shifted by the FAST algorithm in each node linearly in-ternal nodes is not very different from the reconstruction
                                                              creases. As a result, Figure 3(a) shows the insertion per-of the whole index. We believe one of the most important
                                                    formance degrades with larger tree node sizes.beneﬁts of using PM is the instantaneous recoverability.
                                                       Regarding search performance, Figure 3(b) shows theWith FP-Tree, such an instantaneous recovery is impos-
                                                         binary search performance improves as we increase the
   1Codes are available at https://github.com/DICL/FAST FAIR          tree node size because of the smaller tree height and the




194  16th USENIX Conference on File and Storage Technologies                      USENIX Association

<!-- PAGE 10 -->

                                                                                                                    S                                                                                     F:                                                              FAST+FAIR                                                                                                 L:                                                                                   FAST+Logging               Linear                    Search                                                      0.5                                                       20                    Search                                                                                                                            (usec)   0.6                                                                                     P:                                                                                       FP-tree                                                                       W:                                                                                              wB+-tree      (usec)  54       Binary                                                      0.4
                                                                       O: WORT        S: Skiplist                                                                                  Time   0.3    Time  3                                                                                                         S    W                                                       15
      2                                                      0.2                                                                    Linear Search                               (usec)
      1                                                      0.1                                                                 Binary                                                                       Search                                                            W                                                                                                    L                                                                                                                                                Elapsed       Elapsed                                                                                                            F                                              0      0                                                                                                               P  O
                                                256B512B                                                 1KB 2KB                                                        4KB          256B 512B 1KB 2KB 4KB                                                                                                 F L P                                                                               O                                                                                                       Time 10          S    W  S            Node Size (Bytes)                     Node Size (Bytes)               5             F L P  O                                                                           F L PWO
         (a) Insertion Time                  (b) Search Time                0
                                                                    120/120(DRAM) 300/300     600/600     900/900
          Figure 3: Linear vs. Binary Search
                                                                     Read/Write Latency (nsec)
                                                                                            clflush                 Search         Node Update
         25
                     FAST+FAIR      WORT                   Figure 5: Breakdown of Time Spent for B-tree Inser-
         20                 FP-tree            Skiplist                              wB+-tree                                   tion (AVG. of 5 Runs)
         15
                 Speedup   10
                                                                ative performance improvement over SkipList for range
          5
                                                      query performance with various selection ratios. The se-
          0                                                  lection ratio is the proportion of selected data to the num-
                 0.1    0.5    1     3     5
                                                         ber of data in an index. We set the read latency of PM                         Selection Ratio(%)
                                                               to 300 nsec and inserted 10 million 8-byte random in-
Figure 4: Range Query Speed-Up from using SkipList                                                             teger keys into 1 KB tree nodes. A B+-tree with FAST
with Varying Selection Ratio (AVG. of 5 Runs)                                                   and FAIR processes range queries up to 20 times faster
                                                         than SkipList and consistently outperforms other persis-
                                                                 tent indexing structures (6∼27% faster than FP-tree andfewer number of key comparisons. However, the binary
                                         25∼33% faster than wB+-tree) due to its simple structuresearch often stalls due to poor cache locality and the
                                                   and sorted keys.failed branch prediction. As a result, it performs slower
than the linear search when the tree node size is smaller
than 4 KB. Although the linear search accesses more    6.4 PM Latency Effect
cache lines and incurs more LLC cache misses, memory-    In the experiment shown in Figure 5 and  6, we index
level parallelism (MLP) helps read multiple cache lines    10 million 8 byte key-value pairs in uniform distribution
at the same time. As a result, the number of effective LLC    and measure the average time spent per query while in-
cache misses of the linear search is smaller than that of    creasing the read and write latency of PM from 300 nsec.
the binary search.                                      For FP-tree, we set the size of the leaf nodes and inter-
  Overall, our B+-tree implementation shows good in-    nal nodes to 1 KB and 4 KB respectively as was done in
sertion and search performance when the tree node size     [34]. The node size of wB+-tree is ﬁxed at 1 KB because
is 512 bytes and 1 KB. Hence, we set the B+-tree node    each node can hold no more than 64 entries. Both conﬁg-
size to 512 bytes for the rest of the experiments unless    urations are the fastest performance settings for FP-tree
stated otherwise. Note that the 512-byte tree node size    and wB+-tree [34].
occupies only eight cache lines, thus FAST requires eight                                                          Figure 5 shows that FAST+FAIR, FP-tree, and WORT
cache line ﬂushes in the worst case and four cache line                                               show comparable insertion performances and they out-
ﬂushes on average. Since the binary search makes lock-                                                     perform wB+-tree and SkipList by a large margin. In de-
free search impossible and the linear search performs                                                                                tail, the insertion time is composed of three parts, Cache
faster than binary search when the node size is small, we     line ﬂush, Search, and Node Update times. wB+-tree
use the linear search for the rest of the experiments.                                                                   calls a 1.7 times larger number of cache line ﬂushes than
                                             FAST+FAIR. We also measured the performance of a
6.3  Range Query                              FAST-only B+-tree with legacy tree rebalancing opera-
A major reason to use B+trees instead of hash tables is to     tions that have a logging overhead. Because of the log-
allow range queries. Hash indexes are known to perform    ging overhead, FAST+Logging performs 7∼18% slower
well for exact match queries, but they cannot be used if    than FAST+FAIR.
a query involves the ordering of data such as ORDER BY     The poor performance of SkipList is because of its
sorting and MIN/MAX aggregation. Thus many commer-    poor cache locality. Without clustering similar keys in
cial database systems use a hash index as a supplemen-    contiguous memory space and exploiting the cache lo-
tary index of the B+-tree index.                                  cality, byte-addressable in-memory data structures such
  In the experiments shown in Figure 4, we show the rel-    as SkipList, radix trees, and binary trees fail to lever-




USENIX Association                            16th USENIX Conference on File and Storage Technologies  195

<!-- PAGE 11 -->

                                       12.382us  19.014us
       8                                                     9                                                                                                   12                                                                                              wB+-tree                                                             FAST+FAIR            FAST+FAIR                                                                                                    WORT                                                                                                             FAST+FAIR                                                     8       7                                                              WORT                      FP-tree                                                                                                                                                             FP-tree           Skiplist                                                                                                   10                                                     7  FAST+Logging                                                                                           FP-tree                                                                                                                      Skiplist       6                 wB+-tree                                                                                                                                          wB+-tree
       5     WORT                                  6                                              8         (usec)                                                                                                                                                                                                                                                                                          (usec)                                                                                                                                                 (usec)  5       4          Skiplist                                                                                                    6                                                     4
                                                     3                                                                                                    4                                                                                                 Time                                                                                                                                                                                            Time      Time  32                                                     2                                                                                                    2       1                                                     1
       0                                            0                                              0
       120(DRAM) 300     600     900             120(DRAM) 300     600     900              700(DRAM) 1000    1300    1600

           PM read latency (nsec)                  PM write latency (nsec)                   PM write latency (nsec)

        (a) Search : Varying Read Latency       (b) Insert : Varying Write Latency on TSO Ar- (c) Insert : Varying Write Latency on Non-TSO
                                                  chitecture                                    Architecture
                  Figure 6: Performance Comparison of Single-Threaded Index (AVG. of 5 Runs)


age memory level parallelism. Hence, block-based data    6.5  Performance on Non-TSO
structures such as B+-trees that beneﬁt from clustered    Although stores-after-stores are not reordered in X86 ar-
keys need to be considered not only for block device     chitectures, ARM processors do not preserve total store
storage systems but also for byte-addressable PM. FP-    ordering. To evaluate that the performance of FAST on
tree beneﬁts from faster access to internal nodes than   non-TSO architectures, we add dmb instruction as a
FAST+FAIR and WORT, but  it calls a slightly larger   memory barrier to enforce the order of store instructions
number of cache line ﬂushes than FAST+FAIR (4.8 v.s    and measure the insertion performance of FAST and
4.2) because of ﬁngerprints and leaf-level logging.        FAIR on a smartphone, Nexus 5, which has a 2.26 GHz
  Figure 6(a) shows how the read latency of PM af-    Snapdragon 800 processor and 2 GB DDR memory. To
fects the exact match query performance. FP-tree shows    emulate PM, we assume that a particular address range
a slightly faster search performance than FAST+FAIR    of DRAM is PM and the read latency of PM is no differ-
when the read latency is higher than 600 nsec because    ent from that of DRAM. We emulate the write latency of
it has faster access to volatile internal nodes. When the   PM by injecting nop instructions. Since the word size
read latency is 900 nsec, WORT spends twice as much    of Snapdragon 800 processor is 4 bytes, the granular-
time as FAST+FIAR to search the index. Interestingly,     ity of failure-atomic writes is 4 bytes, and the node size
the average number of LLC cache misses of WORT that    of wB+-tree and FP-tree is limited to 256 bytes accord-
we measured with perf is 4.9 while that of FAST+FAIR is     ingly. Figure  6(c) shows that, when the PM latency is
8.3 in the experiments. Although B+-tree variants have a    the same as that of DRAM, our proposed FAST+FAIR
larger number of LLC cache misses than WORT, mostly    shows worse performance than FP-tree although FP-tree
they access adjacent cache lines and beneﬁt from serial     calls more cache line ﬂush instructions. This is because
memory accesses, i.e., hardware prefetcher and memory   FAST+FAIR calls more memory barrier instructions than
level parallelism. To reﬂect the characteristics of modern    FP-tree on ARM processors (16.2 vs. 6.6). However, as
hardware prefetcher and memory-level parallelism and    the write latency of PM increases, FAST+FAIR outper-
to avoid overestimation of the overhead of serial memory    forms other indexes because the relative overhead of dmb
accesses, Quartz counts the number of memory stall cy-    becomes less signiﬁcant compared to that of the cache
cles for each LOAD request and divides it by the memory     line ﬂushes (dccmvac). In our experiments, the perfor-
latency to count the number of serial memory accesses   mance of FAST+FAIR is up to 1.61 times faster than
and estimate the appropriate read latency for them [43].    wB+-tree.
Therefore, the search performances of B+-tree variants
are less affected by the increased read latency of PM    6.6  TPC-C Benchmark
compared to WORT and SkipList.                          In real-world applications, workloads are often mixed
  In the experiments shown in Figure 6(b), we mea-    with writes and reads. In the experiments shown in Fig-
sure the average insertion time for the same batch in-    ure 7, we evaluate the performance using TPC-C bench-
sertion workload while increasing only the write latency    mark. TPC-C benchmark is an OLTP benchmark that
of PM. As we increase the write latency, WORT, which     consists of 5 different types of queries (New-Order, Pay-
calls fewer cache line ﬂushes than FAST+FAIR, outper-    ment, Order-Status, Delivery, and Stock-Level). We var-
forms all other indexes because the number of cache line    ied the percentage of these queries to generate four dif-
ﬂushes becomes a dominant performance factor and the     ferent workloads so that the proportion of search queries
poor cache locality of WORT gives less impact on the    increases from W1 to W4. The read and write latency of
performance. FAST+FAIR consistently outperforms FP-   PM are both set to 300 nsec. FAST+FAIR consistently
tree, wB+-tree, and Skip List as it calls a lower number    outperforms other indexes because of its good insertion
of clflush instructions.                              performance and superior range query performance due




196  16th USENIX Conference on File and Storage Technologies                      USENIX Association

<!-- PAGE 12 -->

                                                         read threads can tolerate such inconsistent tree nodes. In         60
              FAST+FAIR                        WORT                        the lock-free concurrency experiments, a large number         50                         FP-tree           Skiplist
         40      wB+-tree                                    of read transactions access various partially updated tree                        (Kops/sec)
         30                                             nodes. If the read transactions can ignore such transient
         20                                                 inconsistent tree nodes, instant recovery is possible.
         10                                                 In the experiments shown in Figure 8, we run three                        Throughput    0                                          workloads - 50M Search, 50M Insertion, and Mixed. We
          W1   W2   W3   W4                 insert 50 million 8 byte random keys into the index and
                 TPCC Workloads                    run each workload: For 50M Insertion workload, we in-
                                                                    sert additional 50 million keys into the index. For 50M
Figure 7: TPC-C benchmark (AVG. of 5 Runs)  : W1    Search we search 50 million keys. And for Mixed work-
[NewOrder 34%, Payment 43%, Status 5%, Delivery 4%, StockLevel     load, each thread alternates between four insert queries,
14%], W2 [27%, 43%, 15%, 4%, 11%], W3 [20%, 43%, 25%, 4%,    sixteen search queries, and one delete query. We use nu-
8%], W4 [13%, 43%, 35%, 4%, 5%]                            mactl to bind threads explicitly to a single socket to min-
                                                       imize the socket communication overhead and we dis-
                                                                  tribute the workload across a number of threads.to the sorted data in its leaf nodes. While WORT shows
                                                          Figure 8(a) shows that FAST+FAIR gains about athe fastest insertion performance, it suffers from a poor
                                                      11.7x faster speedup when the number of threads in-range query performance in this benchmark.
                                                           creases from one to sixteen. However, the speed-up
6.7  Concurrency and Recoverability             saturates over 16 threads because our testbed machine
                                                       has 16 vCPUs in a single socket. For FP-tree and B-In the experiments shown in Figure 8, we evaluate the
                                                               link tree, the search speed-up becomes saturated whenperformance of multi-threaded versions of FAST+FAIR,
                                          we use 8 and 4 threads respectively. When we run 8FP-tree, SkipList, and B-link tree. Although wB+-tree
                                                              threads, FP-tree takes advantage of the TSX and showsand WORT  are  not designed  to handle concurrent
                                                       a throughput about 2.2x higher than B-link tree. Sincequeries, they can employ well-known concurrency con-
                                                          SkipList also beneﬁts from lock-free search, it scales totrol protocols such as the crabbing protocol [41]. How-
                                                 16 threads but from a much lower throughput. Althoughever, we do not implement and evaluate multi-threaded
                                              FAST+FAIR+LeafLocks requires read threads to acquireversions of wB+-tree and WORT. Instead, we present
                                                         read locks in leaf nodes, FAST+FAIR+LeafLocks showsthe performance of B-link tree for reference because
                                                       a comparable concurrency level with FAST+FAIR. Noteit is known that B-link tree outperforms the crabbing
                                                                 that the lock-free FAST+FAIR operates at read uncom-protocol [41]. Note that B-link tree is not designed to
                                                         mitted mode while FAST+FAIR+LeafLocks operates atprovide the failure-atomicity for byte-granularity writes
                                                                serializable mode.in PM and B-link tree does not allow the lock-free
                                                             In terms of write performance, FP-tree does not bene-search. We implemented the concurrent version of FP-
                                                          ﬁt much from the TSX as it shows a similar performancetree using Intel’s Transactional Synchronization Exten-
                                                               to B-link tree. It is because FP-tree performs expensivesion (TSX) as was done in [34]. For this experiments,
                                                        logging when a leaf node splits although it beneﬁts fromthe write latency of PM is set to 300 nsec and the read
                                                            the faster TSX-enabled lock. In Figure 8(b), FAST+FAIRlatency of PM is set to be equal to that of DRAM.
                                                         achieves about 12.5x higher insertion throughput whenFAST+FAIR and SkipList eliminate the necessity of read
                                                 16 threads run concurrently. In contrast, FP-tree and B-locks but they require write locks to serialize write opera-
                                                               link tree achieve only a 7.7x and 4.4x higher through-tions on tree nodes. Our implementations of FAST+FAIR
                                                                put. Figure 8(c) shows that the scalability of FP-treeand B-link tree use std::mutex class in C++11 and
                                                   and B-link tree is limited because of read locks whileSkiplist uses a spin lock with gcc built-in CAS function,
                                           FAST+FAIR takes advantage of the lock-free search. sync bool compare and swap. But they can also
                                                     For the mixed workload, FAST+FAIR achieves up to aemploy the hardware transactional memory for higher
                                                     11.44x higher throughput than a single thread.concurrency. We compiled these implementations with-
out the -O3 optimization option because the compiler op-
timization can reorder instructions and it affects the cor-   7  Related Work
rectness of lock-free algorithm.                          Lock-free index: In  parallel computing community,
  Although these experiments are intended to evaluate    various non-blocking implementations of popular data
the concurrency, they also show the instant recoverabil-     structures such as queues, lists, and search trees have
ity of FAST and FAIR algorithms. In a physical power-    been studied [4, 14, 15, 32]. Among various lock-free
off test, we need to generate a large number of partially    data structures, Braginsky et al.’s lock-free dynamic B+-
updated transient inconsistent tree nodes and see whether     tree [4] and Levandoski’s Bw-tree [28] are the most rel-




USENIX Association                            16th USENIX Conference on File and Storage Technologies  197

<!-- PAGE 13 -->

       25600                                                6400                                                 12800
       12800                                                3200                                                  6400
        6400                                                                                                                  3200                                                            1600            (Kops/sec)        3200                                                                                                                                                                         (Kops/sec)                                                                                                                                                                                                                                                                                                                                                                                                                                     (Kops/sec)                                                                                                                  1600
        1600                                                 800
                                                                                                                   800
                                                                                                                                                               FAST+FAIR         800                                         FAST+FAIR                                                             400                                                                                                                                                                    FAST+FAIR+LeafLock                                                                                                   FAST+FAIR                                                                                                                   400                                    FAST+FAIR+LeafLock         400                                                                                                                                               FP-tree                                                                                                                                                                                                                                   FP-tree                                                              FP-tree                                                             200                                                                                                                                               B-link                                                                                                                                                                                                                                 B-link                                                                                                                   200                                                               B-link         200
                                                                                                                                                              Skiplist                                                                                                                                                                                                                                                          Skiplist                                                                    Skiplist            Throughput    100                                                                                                                                                                         Throughput   100                                                                                                                                                                             Throughput    100
                  1    2    4    8    16   32                         1    2    4    8    16   32                          1    2    4    8    16   32
                    # of Threads                                # of Threads                                 # of Threads

                   (a) 50M Search                                 (b) 50M Insert                    (c) 200M Search/50M Insert/12.5M Delete

                    Figure 8: Performance with Varying Number of Threads (AVG. of 5 Runs)


evant to our work. Unlike their lock-free B+-tree imple-   (RTM) and Hardware Lock Elision (HLE) can be used
mentations, our current design still requires write latches     to support coarse-grained atomic cache line writes [13,
for write threads because persistent B+-trees need to    27, 39, 46, 47]. Hardware transactional memories guar-
address durability as an additional challenge. We leave    antee a dirty cache line remains in the write combin-
lock-free writes for persistent index as a future work.       ing store buffer so that isolation can be preserved. How-
  Memory persistency: In order to resolve the ordering     ever, memory persistency models including even strict
issues of memory writes in PM-based systems, numer-    persistency do not guarantee multiple cache lines will be
ous works [9, 35, 29] have lately proposed novel mem-    ﬂushed atomically even with the help of hardware trans-
ory persistency models, such as strict persistency [35]     actional memory [39]. Hence, if a system crashes while
and epoch persistency [9]. Strict persistency does not    ﬂushing multiple cache lines, its consistency can not be
distinguish memory consistency from memory persis-    guaranteed. Hence, hardware transactional memory in its
tency, but epoch persistency [9] requires persist barri-    current form cannot replace our FAST and FAIR algo-
ers so that persist order may deviate from the volatile    rithms as long as the tree node size is larger than a single
memory order. These persistency models complement    cache line [22] and a tree needs rebalancing operations.
our work. FAST and FAIR guarantee the consistency of
B+-tree under strict persistency model. If memory con-
                                         8  Conclusionsistency is decoupled from memory persistency and the
persist order deviates from the volatile memory order as    In this work, we have designed, implemented, and evalu-
in relaxed persistency, FAST and FAIR must call a per-    ated Failure-Atomic ShifT (FAST) and Failure-Atomic
sist barrier for every cache line ﬂush instruction because    In-place Rebalance (FAIR) algorithms for legacy B+-
they must enforce the order of cache line ﬂushes to PM.     trees to get the most beneﬁt out of byte-addressable per-
However, within the same cache line, we do not need to     sistent memory. FAST and FAIR solves the granular-
call a persist barrier for each shift operation because ar-     ity mismatch problem of PM without using logging and
ray elements in the same cache line are guaranteed to be    without modifying the data structure of B+-trees.
ﬂushed to PM even under the relaxed persistency model.     FAST and FAIR algorithms transform a consistent B+-
The only condition that FAST and FAIR require is that     tree into another consistent state or a transient incon-
the dirty cache lines must be ﬂushed in order. There-     sistent state that read operations can endure. By mak-
fore, FAST and FAIR place minimal persistence over-    ing read operations tolerate transient inconsistency, we
head under both strict and relaxed persistency models.    can avoid expensive copy-on-write and logging. Besides,
That is, FAST calls persist barriers only as many times   we can isolate read transactions, which enables non-
as the number of dirty cache lines in a B+-tree node un-    blocking lock-free search.
der the relaxed persistency. On the other hand, other per-
sistent indexes such as wB+-tree and FP-tree that em-
                                       Acknowledgementploy append-only strategy need to call a persist barrier
for each store instruction since their store instructions are   We would like to give our special thanks to our shep-
not dependent. Due to the unavailability of PM that im-    herd Dr. Jorge Guerra and the anonymous reviewers.
plements various persistency models, we leave a perfor-    Their comments and suggestions helped us correct and
mance evaluation of our FAST and FAIR schemes under    improve this work. This research was supported by Sam-
relaxed persistency model to our future work              sung Research Funding Centre of Samsung Electronics
  Hardware transactional memory: The advent of    under Project Number SRFCSRFC-IT1501-04. The ﬁrst
commercially available hardware transactional memory    and the second authors contributed equally and the cor-
such as the Intel’s Restricted Transactional Memory    responding author is Beomseok Nam.




198  16th USENIX Conference on File and Storage Technologies                      USENIX Association

<!-- PAGE 14 -->

References                                                    29th ACM Symposium on Principles of Distributed Com-
                                                                     puting (PODC) (2010).
 [1] HP  Enterprise  Lab,  Memory  Driven  Computing.
                                                                 [15] FOMITCHEV, M., AND RUPPERT, E.  Lock-free linked
     https://www.labs.hpe.com/next-next/mdc.
                                                                                           lists and skiplists. In the 23rd ACM Symposium on Prin-
 [2]  Intel and Micron produce breakthrough memory technol-           ciples of Distributed Computing (PODC) (2004).
     ogy. https://newsroom.intel.com/news-releases/intel-and-
                                                                 [16] HU, Q., REN, J., BADAM, A., AND MOSCIBRODA, T.     micron-produce-breakthrough-memory-technology.
                                                                     Log-structured non-volatile main memory.  In Proceed-
 [3] ARULRAJ, J., PAVLO, A., AND DULLOOR, S. R. Let’s          ings of the USENIX Annual Technical Conference (2017).
      talk about storage & recovery methods for non-volatile
                                                                 [17] HUAI, Y.  Spin-transfer torque MRAM (STT-MRAM):    memory database systems.  In Proceedings of the 2015
                                                                  Challenges and prospects. AAPPS bulletin 18, 6 (2008),   ACM SIGMOD International Conference on Manage-
                                                                33–40.     ment of Data (2015), ACM, pp. 707–722.
 [4] BRAGINSKY, A., AND PETRANK, E. A lock-free B+tree.     [18] INTEL.    Intel R⃝64 Architecture Memory Ordering
     In 24th ACM Symposium on Parallelism in Algorithms         White Paper, August 2007. SKU 318147-001.
    and Architectures (SPAA) (2012).                            [19] KIM, C., CHHUGANI,  J., SATISH, N., SEDLAR, E.,
 [5] CHEN, S., AND JIN, Q.  Persistent B+-Trees in non-       NGUYEN,  A.  D., KALDEWEY,  T.,  LEE,  V.  W.,
      volatile main memory. Proceedings of the VLDB Endow-        BRANDT, S. A., AND DUBEY, P. FAST: Fast architec-
     ment (PVLDB) 8, 7 (2015), 786–797.                               ture sensitive tree search on modern CPUs and GPUs. In
                                                                 Proceedings of 2010 ACM SIGMOD International Con-
 [6] CHI, P., LEE, W.-C., AND XIE, Y. Making B+-tree efﬁ-
                                                                       ference on Management of Data (SIGMOD) (2010).
      cient in PCM-based main memory. In Proceedings of the
    2014 international symposium on Low power electronics     [20] KIM, W.-H., KIM, J., BAEK, W., NAM, B., AND WON,
    and design (2014), ACM, pp. 69–74.                            Y. NVWAL: Exploiting NVRAM in write-ahead logging.
                                                                      In 21st International Conference on Architectural Sup-
 [7] CHIDAMBARAM, V., SHARMA, T., ARPACI-DUSSEAU,
                                                                      port for Programming Languages and Operating Systems
    A. C., AND ARPACI-DUSSEAU, R. H. Consistency with-
                                                        (ASPLOS) (2016).
     out ordering. In Proceedings of the 11th USENIX confer-
     ence on File and Storage Technologies (FAST) (2012).        [21] KIM, W.-H., NAM, B., PARK, D., AND WON, Y. Re-
                                                                      solving journling of journal anomaly in Android I/O: [8] CHONG, N., AND ISHTIAQ, S.  Reasoning about the
                                                                     Multi-version B-tree with lazy split.  In Proceedings of   ARM weakly consistent memory model.  In Proceed-
                                                                        the 11th USENIX conference on File and Storage Tech-     ings of the 2008 ACM SIGPLAN workshop on Memory
                                                                     nologies (FAST) (2014).     systems performance and correctness (MSPC’08) (2008),
    ACM, pp. 16–19.                                           [22] KIM, W.-H., SEO, J., KIM, J., , AND NAM, B. clfB-tree:
                                                                  Cacheline friendly persistent B-tree for NVRAM. ACM [9] CONDIT, J., NIGHTINGALE, E. B., FROST, C., IPEK,
                                                                    Transactions on Storage (TOS), Special Issue on NVM      E., LEE, B. C., BURGER, D., AND COETZEE, D. Bet-
                                                           and Storage (2018).      ter I/O through byte-addressable, persistent memory. In
     Proceedings of the 22nd ACM Symposium on Operating     [23] LEE, S. K., LIM, K. H., SONG, H., NAM, B., AND
     Systems Principles (SOSP) (2009).                       NOH, S. H. WORT: Write optimal radix tree for per-
[10] CUPPU, V., JACOB, B., DAVIS, B., AND MUDGE, T. A            sistent memory storage systems.  In Proceedings of the
                                                                  15th USENIX conference on File and Storage Technolo-     performance comparison of contemporary DRAM archi-
      tectures.  In the 26th International Symposium on Com-           gies (FAST) (2017).
     puter Architecture (1999).                                   [24] LEE, W., LEE, K., SON, H., KIM, W.-H., NAM, B.,
[11] DANOWITZ, A., KELLEY, K., MAO, J., STEVENSON,       AND WON, Y. WALDIO: Eliminating the ﬁlesystem jour-
       J. P., AND HOROWITZ, M. Cpu db: recording micropro-          naling in resolving the journaling of journal anomaly. In
     cessor history. Communications of the ACM 55, 4 (2012),         Proceedings of the 2015 USENIX Anual Technical Con-
     55–63.                                                           ference (2015).
[12] DONG, M., AND CHEN, H.  Soft updates made sim-     [25] LEHMAN, P. L., AND YAO, S. B. Efﬁcient locking for
     ple and fast on non-volatile memory.  In 2017 USENIX          concurrent operations on B-trees. ACM Transactions on
     Annual Technical Conference (Santa Clara, CA, 2017),         Database Systems 6, 4 (1981), 650–670.
    USENIX Association, pp. 719–731.                         [26] LEHMAN, T. J., AND CAREY, M. J. A study of index
[13] DULLOOR, S. R., KUMAR, S., KESHAVAMURTHY, A.,           structures for main memory database management sys-
    LANTZ, P., REDDY, D., SANKARAN, R., AND JACK-          tems.  In Proceedings of the 12th International Confer-
    SON, J. System software for persistent memory. In Pro-         ence on Very Large Data Bases (VLDB) (1986).
     ceedings of the 9th ACM European Conference on Com-                                                                 [27] LEIS,  V., KEMPER, A., AND NEUMANN, T.   Ex-
     puter Systems (EuroSys) (2014), pp. 15:1–15:15.                                                                           ploiting hardware transactional memory in main-memory
[14] ELLEN, F., FATAOUROU, P., RUPPERT, E., AND VAN          databases. In Proceedings of the 30th International Con-
    BREUGEL, F. Non-blocking binary search trees. In the          ference on Data Engineering (ICDE) (2014).




USENIX Association                            16th USENIX Conference on File and Storage Technologies  199

<!-- PAGE 15 -->

[28] LEVANDOSKI, J. J., LOMET, D. B., AND SENGUPTA,     [41] SILBERSCHATZ, A., KORTH, H., AND SUDARSHAN, S.
     S. The Bw-Tree: a B-tree for new hardware platforms.         Database Systems Concepts. McGraw-Hill, 2005.
     In Proceedings of the 29th International Conference on                                                                 [42] VENKATARAMAN, S., TOLIA, N., RANGANATHAN, P.,
    Data Engineering (ICDE) (2013).                                                AND CAMPBELL, R. H.  Consistent and durable data
[29] LU, Y., SHU, J., AND SUN, L.  Blurred persistence in           structures for non-volatile byte-addressable memory. In
      transactional persistent memory.  In Proceedings of the          9th USENIX conference on File and Storage Technologies
     31st International Conference on Massive Storage Sys-         (FAST) (2011).
     tems and Technology (MSST) (2015).                                                                 [43] VOLOS, H., MAGALHAES, G., CHERKASOVA, L., AND
[30] MCKENNEY, P. E. Memory ordering in modern micro-          LI, J.  Quartz: A lightweight performance emulator for
     processors. Linux Journal 136, Aug. (2005).                        persistent memory software. In 15th Annual Middleware
                                                                Conference (Middleware ’15) (2015).
[31] MCKUSICK, M. K., GANGER, G. R., ET AL.   Soft
     updates: A technique for eliminating most synchronous     [44] VOLOS,  H.,  TACK,  A.  J., AND  SWIFT, M. M.
     writes in the fast ﬁlesystem. In USENIX Annual Techni-        Mnemosyne: Lightweight persistent memory. In 16th In-
     cal Conference, FREENIX Track (1999), pp. 1–17.                 ternational Conference on Architectural Support for Pro-
                                                         gramming Languages and Operating Systems (ASPLOS)
[32] MICHAEL, M. M. High performance dynamic lock-free                                                                      (2011).
     hash tables and list-based sets.  In the 14th ACM Sym-
     posium on Parallelism in Algorithms and Architectures     [45] WANG, Y., KAPRITSOS, M., REN, Z., MAHAJAN, P.,
     (SPAA) (2002).                                    KIRUBANANDAM,  J., ALVISI, L., AND DAHLIN, M.
                                                                 Robustness in the salus scalable block store. In Proc. of
[33] MORARU, I., ANDERSEN, D. G., KAMINSKY, M., TO-       USENIX NSDI 2013 (Apr 2013).
     LIA, N., BINKERT, N., AND RANGANATHAN, P. Con-
                                                                 [46] WANG, Z., QIAN, H., LI, J., AND CHEN, H.  Using      sistent, durable, and safe memory management for byte-
                                                                               restricted transactional memory to build a scalable in-     addressable non volatile main memory.  In Proceedings
                                                     memory database.  In ACM SIGOPS/Eurosys European      of the ACM Conference on Timely Results in Operating
                                                                Conference on Computer Systems (EuroSys) (2014).     Systems (TRIOS) (2013).
                                                                 [47] WEI, X., SHI, J., CHEN, Y., CHEN, R., AND CHEN, H.
[34] OUKID, I., LASPERAS, J., NICA, A., WILLHALM, T.,
                                                                       Fast in-memory transaction processing using RDMA and
    AND LEHNER, W. FPTree: A hybrid SCM-DRAM per-
                                               HTM. In Proceedings of the 25th Symposium on Operat-
      sistent and concurrent B-tree for storage class memory. In
                                                                     ing Systems Principles (SOSP) (2015).
     Proceedings of 2016 ACM SIGMOD International Con-
     ference on Management of Data (SIGMOD) (2016).          [48] WONG, H.-S.  P., RAOUX, S., KIM, S., LIANG,  J.,
                                                        REIFENBERG, J. P., RAJENDRAN, B., ASHEGHI, M.,
[35] PELLEY, S., CHEN, P. M., AND WENISCH, T. F. Mem-
                                                AND GOODSON, K. E. Phase change memory. Proceed-
     ory persistency. In Proceedings of the 41st International
                                                                      ings of the IEEE 98, 12 (2010), 2201–2227.
    Symposium on Computer Architecture (ISCA) (2014),
     pp. 265–276.                                               [49] YANG, J., WEI, Q., CHEN, C., WANG, C., AND YONG,
                                                         K. L.  NV-Tree: reducing consistency const for NVM-
[36] RAO, J., AND ROSS, K. A. Cache conscious indexing for
                                                                based single level systems.  In Proceedings of the 13th
     decision-support in main memory. In Proceedings of the
                                                   USENIX conference on File and Storage Technologies
     25th International Conference on Very Large Data Bases
                                                              (FAST) (2015).
    (VLDB) (1999).

[37] RAO, J., AND ROSS, K. A. Making B+-trees cache con-
     scious in main memory.  In Proceedings of 2000 ACM
   SIGMOD International Conference on Management of
    Data (SIGMOD) (2000).

[38] RIXNER, S., DALLY, W. J., KAPASI, U. J., MATTSON,
      P., AND OWENS,  J. D.  Memory access scheduling.
     In ACM SIGARCH Computer Architecture News (2000),
      vol. 28, ACM, pp. 128–138.

[39] SEO, J., KIM, W.-H., BAEK, W., NAM, B., AND NOH,
     S. H. Failure-atomic slotted paging for persistent mem-
      ory. In Proceedings of the 22nd International Conference
    on Architectural Support for Programming Languages
    and Operating Systems (ASPLOS) (2017).

[40] SEWELL,  P., SARKAR,  S., OWENS,  S., NARDELLI,
      F. Z., AND MYREEN, M. O. x86-tso: a rigorous and us-
     able programmer’s model for x86 multiprocessors. Com-
     munications of the ACM 53, 7 (2010), 89–97.




200  16th USENIX Conference on File and Storage Technologies                      USENIX Association