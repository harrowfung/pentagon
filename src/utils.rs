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
