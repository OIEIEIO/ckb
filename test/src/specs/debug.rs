use crate::{Node, Spec};

pub struct TestFailedOnCI;

impl Spec for TestFailedOnCI {
    crate::setup!(num_nodes: 4);

    fn run(&self, _: &mut Vec<Node>) {
        panic!("panic to test failed integration tests on ci");
    }
}
