use frustrate::bridge;
use models_shared::model::{Block, Graph, Shape};

#[bridge(sync)]
pub fn make_block(id: i64, text: String) -> Block {
    Block {
        id,
        text,
        tags: vec!["a".into()],
    }
}

#[bridge(sync)]
pub fn describe(graph: Graph) -> String {
    let shape = match graph.shape {
        Shape::Circle { r } => format!("circle {r}"),
        Shape::Square(s) => format!("square {s}"),
        Shape::Empty => "empty".into(),
    };
    let ids: Vec<String> = graph.blocks.iter().map(|b| b.id.to_string()).collect();
    format!("{} {shape} {:?}", ids.join(","), graph.mood)
}
