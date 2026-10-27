use frustrate_macros::bridge;

#[bridge(data)]
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub id: i64,
    pub text: String,
    pub tags: Vec<String>,
}

#[bridge]
impl Block {
    /// A member of the type, which crosses through bridge A only.
    #[bridge(sync)]
    pub fn weight(&self) -> i64 {
        self.text.len() as i64 + self.tags.len() as i64
    }
}

#[bridge(data)]
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Circle { r: f64 },
    Square(f64),
    Empty,
}

#[bridge(data)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    Calm,
    Loud,
}

#[bridge(data)]
#[derive(Clone, Debug, PartialEq)]
pub struct Graph {
    pub blocks: Vec<Block>,
    pub shape: Shape,
    pub mood: Mood,
}
