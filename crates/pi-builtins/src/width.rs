pub(crate) fn scalar_width(c: char) -> usize {
	if matches!(
		c,
		'\u{1160}'..='\u{11FF}' | '\u{D7B0}'..='\u{D7C6}' | '\u{D7CB}'..='\u{D7FB}'
	) {
		0
	} else {
		xutf::width_char(c)
	}
}
