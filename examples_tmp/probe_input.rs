fn main() {
    let t = b"hello world";
    println!("pws11={}", fxrs::input_composer::previous_word_start(t, 11));
    println!("nwe0={} nwe6={}", fxrs::input_composer::next_word_end(t, 0), fxrs::input_composer::next_word_end(t, 6));
}
