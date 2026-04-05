use std::fs;
use odorobo_scheduler::scheduler_actor::gen_openapi_spec;

fn main() -> std::io::Result<()> {
    let doc = gen_openapi_spec();
    fs::write("./openapi_spec.json", doc)
}