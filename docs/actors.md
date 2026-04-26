# Kameo Actors

List of all kameo actors contained in these two crates.

## odorobo-manager:

http server (mvp)
websocket server for serial terminal (mvp)

ip management (future roadmap) (also should be a trait so we can swap this out with netbox or something custom later)

managed hosting provider (hits schedulers to auto manage containers, ansible, docker compose, or anything else, and do it to a new vm, an existing vm, or create a new dedicated server and use it)

scheduler_non-ha (mvp) (needs a scheduler trait so schedulers are similar, should be based on inital scheduler_non_ha)
scheduler_ha (future roadmap)
scheduler_other_clusters (future roadmap)
scheduler_to_other_manager (future roadmap)


# odorobo-agent:

agent actor (mvp) (needs to be a trait so we can swap for other things in the future)
vm actor (mvp) (needs to be a trait so we can swap for other things in the future
