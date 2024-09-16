use xdrgen;

fn main() {
    xdrgen::compile("src/sum.x").expect("xdrgen sum.x");
}