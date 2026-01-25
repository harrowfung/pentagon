pub fn gen_random_id(length: u32) -> String {
    let id: String = Vec::from_iter(
        (0..length)
            .map(|_| {
                let idx = fastrand::usize(0..36);
                char::from_digit(idx as u32, 36).unwrap()
            })
            .collect::<Vec<char>>(),
    )
    .into_iter()
    .collect();

    id
}

pub fn autofix(input: Vec<u8>) -> Vec<u8> {
    if input.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::with_capacity(input.len());
    let mut i = 0;
    let len = input.len();

    while i < len {
        // Find start of line content
        let line_start = i;

        // Skip to end of line or end of input
        while i < len && input[i] != b'\n' {
            i += 1;
        }

        // Now i is either at \n or at end of input
        let mut line_end = i; // points to \n or end

        // Backtrack to remove trailing whitespace
        while line_end > line_start && line_end - 1 < len && is_whitespace(input[line_end - 1]) {
            line_end -= 1;
        }

        // Copy the trimmed line content
        if line_end > line_start {
            result.extend_from_slice(&input[line_start..line_end]);
        }

        // Always ensure we have a newline (unless this was the very last empty line
        // and input didn't end with \n — but most conventions want final newline)
        result.push(b'\n');

        // Skip the \n if we consumed one
        if i < len && input[i] == b'\n' {
            i += 1;
        }
    }

    result
}

#[inline(always)]
fn is_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r')
}
