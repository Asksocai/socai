function(domains, site) {
    const url = window.location.href;
    const hostname = window.location.hostname;
    let length = hostname.length;
    while (length > 0 && hostname[length - 1] === '.') length--;
    // Page scripts can replace URL and prototype methods in the main world.
    // Use native Location fields and comparisons, not page-owned URL parsers
    // or String/Array methods, for this permission check.
    const lower = character => {
        const upper = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', lower = 'abcdefghijklmnopqrstuvwxyz';
        for (let i = 0; i < 26; i++) if (character === upper[i]) return lower[i];
        return character;
    };
    for (let p = 0; p < domains.length; p++) {
        const pattern = domains[p];
        const wildcard = pattern[0] === '*' && pattern[1] === '.';
        const start = wildcard ? 1 : 0, width = pattern.length - start;
        if (!length || length < width || (wildcard ? length === width : length !== width)) continue;
        let matches = true;
        for (let i = 0; i < width; i++) {
            if (lower(hostname[length - width + i]) !== lower(pattern[start + i])) {
                matches = false;
                break;
            }
        }
        if (matches) return;
    }
    throw new Error('current page ' + url + ' is outside site skill ' + site + ' domains');
}
