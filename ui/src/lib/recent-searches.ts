const KEY = 'lossless:recent-searches';
const LIMIT = 8;

export function recentSearches(): string[] {
	try {
		const saved: unknown = JSON.parse(localStorage.getItem(KEY) ?? '[]');
		return Array.isArray(saved)
			? saved.filter((q): q is string => typeof q === 'string' && !!q.trim()).slice(0, LIMIT)
			: [];
	} catch {
		return [];
	}
}

export function rememberSearch(query: string): void {
	const q = query.trim();
	if (!q) return;
	try {
		localStorage.setItem(KEY, JSON.stringify([
			q, ...recentSearches().filter((old) => old.toLowerCase() !== q.toLowerCase())
		].slice(0, LIMIT)));
	} catch {
		// Search still works when local storage is unavailable.
	}
}
