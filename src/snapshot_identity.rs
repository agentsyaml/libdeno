const EXPECTED: &[u8] = env!("LIBDENO_EXPECTED_V8_VERSION").as_bytes();
const ACTUAL: &[u8] = deno_core::v8::VERSION_STRING.as_bytes();

const fn equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

const _: () = assert!(
    equal(EXPECTED, ACTUAL),
    "libdeno snapshot V8 version mismatch"
);
