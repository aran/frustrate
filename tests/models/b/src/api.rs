use frustrate::bridge;
use models_shared::model::{Block, Graph, Mood, Shape};

#[bridge(sync)]
pub fn echo(block: Block) -> Block {
    block
}

/// The block and a successor, in a graph of the given shape.
#[bridge(sync)]
pub fn wrap(block: Block, shape: Shape) -> Graph {
    let next = Block { id: block.id + 1, text: format!("{}!", block.text), tags: vec![] };
    Graph { blocks: vec![block, next], shape, mood: Mood::Loud }
}
