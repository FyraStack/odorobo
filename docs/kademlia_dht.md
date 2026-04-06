# kademlia dht

the cluster uses a dht for node lookups and for any global state we store

### quorum/replication factor

We will use majority quorum, but we have to be careful about if we lose enough nodes. 

basically if a server goes down and we only run dht actors/nodes on the compute nodes, we currently would lose 1/8 of the cluster.

This means we need to make sure that our replication factor is high enough that the percent chance of any key in the dht not losing majority quorum

this will be less of a problem as we have more servers that can run dht nodes. one thing that may help is running some dht nodes on the 