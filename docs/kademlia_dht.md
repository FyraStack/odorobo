# kademlia dht

the cluster can use a dht for node lookups and for any global state we store. 

we should have someone (probably caleb) look at it for like an hour or two because if it works it could be possibly faster, definitely more reliable, and easier then a centralized solution like postgres. if they cant get it working in 2h, just stop working on it entirely and it becomes a post MVP thing.

To do this, basically implement what is in the following example into the connect_to_swarm function: https://github.com/libp2p/rust-libp2p/blob/master/examples/ipfs-kad/src/main.rs.

One problem with this is to bootstrap, we either also need mdns for the first nodes, or to hardcode a couple nodes.

### quorum/replication factor

We will use majority quorum, but we have to be careful about if we lose enough nodes. 

basically if a server goes down and we only run dht actors/nodes on the compute nodes, we currently would lose 1/8 of the cluster.

This means we need to make sure that our replication factor is high enough that the percent chance of any key in the dht not losing majority quorum

this will be less of a problem as we have more servers that can run dht nodes. one thing that may help is running some dht nodes on the storage nodes.

The spec recommends a replication factor of 20. https://github.com/libp2p/specs/blob/master/kad-dht/README.md#replication-parameter-k

That is likely high enough that we might not even need to do the math.
