# scheduler routes

this is a very rough definition of what routes we will have

## vms

we need CRUD. we keep track of if its active.

MACs for VMs need to be stored somewhere. it can be either in the DHT or in the Dashboard. doesnt matter.

## volumes

we again need CRUD, and we keep track of the volumes cause we want to just check what is in the SAN

## coloc

we need CRUD for ip to mac address assignment. this will be used in the admin management portal by infra people (ex: kathrine) to assign an ip to coloc when they rack servers.

the dashboard needs to keep track of both coloc and vps ip assignments because we don't want to have to get all the data from the router everytime we do an ip assignment. we still need possibly two routes to get all mac/ip assignments from the router and a single assignment, to be able to verify the data is accurate in the dashboard db. there likely should be a job that runs every so often that verifies all this info and makes sure nothing is out of date.

In addition, we would like the ability to get a list of all MACs on the router, and then filter them by the known MACs (VPSes, our servers, and known colocs), and then display only the unknown macs on the dashboard. this should make it very easy for infra people to rack a server, they see the MAC that we have never seen before and hopefully has a first seen time associated of right now, and then they just use that one when they do the server has been racked admin managment form

## other various things to do

socket terminal stuff via websockets

drain server so infra people can migrate all VMs off a server so we can take it down for maintence

they also need a get all servers so they can view the list of servers. this needs to be able show data from the infra config file, and possibly will give any metrics we can reasonably get from sys_info. the metrics/extra info should possibly be a different route. possibly ask addison

## removing openapi spec

the openapi spec is a pain because of utopia, so addison has told us that we don't need to give her a full spec file. If we just give her a file of rust structs, labeled similar to <ROUTE><Response/Request> she is fine with that.