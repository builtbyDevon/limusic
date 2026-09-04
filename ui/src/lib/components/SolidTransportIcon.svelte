<script lang="ts">
	type TransportIcon = 'pause' | 'previous' | 'next';

	let { kind, class: cls = '' }: { kind: TransportIcon; class?: string } = $props();

	const paths: Record<TransportIcon, string> = {
		pause:
			'M216 48V208a16 16 0 0 1-16 16h-40a16 16 0 0 1-16-16V48a16 16 0 0 1 16-16h40a16 16 0 0 1 16 16ZM96 32H56a16 16 0 0 0-16 16V208a16 16 0 0 0 16 16H96a16 16 0 0 0 16-16V48A16 16 0 0 0 96 32Z',
		previous:
			'M208 47.88V208.12a16 16 0 0 1-24.43 13.43L64 146.77V216a8 8 0 0 1-16 0V40a8 8 0 0 1 16 0v69.23L183.57 34.45A15.95 15.95 0 0 1 208 47.88Z',
		next:
			'M208 40V216a8 8 0 0 1-16 0V146.77L72.43 221.55A15.95 15.95 0 0 1 48 208.12V47.88A15.95 15.95 0 0 1 72.43 34.45L192 109.23V40a8 8 0 0 1 16 0Z'
	};
</script>

<svg viewBox="0 0 256 256" fill="currentColor" class={cls} aria-hidden="true">
	<path d={paths[kind]} />
</svg>
