<script lang="ts">
	import { HugeiconsIcon } from '@hugeicons/svelte';
	import {
		ArrowUpRight01Icon,
		Cancel01Icon,
		Globe02Icon,
		InstagramIcon,
		NewTwitterIcon,
		WikipediaIcon
	} from '@hugeicons/core-free-icons';
	import * as api from '$lib/api';
	import type { ArtistAbout } from '$lib/api';
	import { getCached, putCached } from '$lib/pagecache';
	type SocialLink = { label: string; url: string; icon: typeof InstagramIcon };

	let {
		artistId,
		name,
		youtubeDescription,
		youtubePhoto,
		subscribers,
		monthlyListeners
	}: {
		artistId: string;
		name?: string;
		youtubeDescription?: string;
		youtubePhoto?: string;
		subscribers?: string;
		monthlyListeners?: string;
	} = $props();

	let root = $state<HTMLElement | null>(null);
	let about = $state<ArtistAbout | null>(null);
	let loading = $state(false);
	let failedPhotos = $state<string[]>([]);
	let lightbox = $state<string | null>(null);

	const key = $derived(`artist-about:${artistId}`);
	const description = $derived(youtubeDescription || about?.description);
	const photos = $derived(
		[...new Set([youtubePhoto, ...(about?.photos ?? [])].filter((p): p is string => !!p))]
			.filter((photo) => !failedPhotos.includes(photo))
			.slice(0, 4)
	);
	const socialLinks = $derived(
		[
			about?.instagramUrl
				? { label: 'Instagram', url: about.instagramUrl, icon: InstagramIcon }
				: null,
			about?.xUrl ? { label: 'X', url: about.xUrl, icon: NewTwitterIcon } : null,
			about?.websiteUrl ? { label: 'Website', url: about.websiteUrl, icon: Globe02Icon } : null,
			about?.wikipediaUrl
				? { label: 'Wikipedia', url: about.wikipediaUrl, icon: WikipediaIcon }
				: null
		].filter((link): link is SocialLink => !!link)
	);
	const visible = $derived(
		!!description || photos.length > 0 || !!subscribers || !!monthlyListeners || socialLinks.length > 0
	);

	async function load(cid: string, artistName: string) {
		const hit = getCached<ArtistAbout>(key);
		if (hit) {
			about = hit;
			return;
		}
		loading = true;
		try {
			const fresh = await api.getArtistAbout(artistName, cid);
			if (cid !== artistId) return;
			about = fresh;
			putCached(`artist-about:${cid}`, fresh);
		} catch {
			// Enrichment is optional. YouTube's own photo, counts, and description still render.
		} finally {
			if (cid === artistId) loading = false;
		}
	}

	$effect(() => {
		const cid = artistId;
		const artistName = name?.trim();
		about = getCached<ArtistAbout>(`artist-about:${cid}`) ?? null;
		failedPhotos = [];
		lightbox = null;
		if (!root || !artistName || about) return;
		const io = new IntersectionObserver(
			(entries) => {
				if (!entries.some((entry) => entry.isIntersecting)) return;
				io.disconnect();
				load(cid, artistName);
			},
			{ rootMargin: '500px' }
		);
		io.observe(root);
		return () => io.disconnect();
	});

	function photoFailed(photo: string) {
		if (!failedPhotos.includes(photo)) failedPhotos = [...failedPhotos, photo];
		if (lightbox === photo) lightbox = null;
	}

	function open(url: string) {
		api.openExternal(url).catch(() => {});
	}
</script>

<svelte:window
	onkeydown={(event) => {
		if (event.key === 'Escape') lightbox = null;
	}}
/>

<div bind:this={root} class={visible ? 'mt-4' : 'h-px'}>
{#if visible}
	<section aria-label={`About ${name ?? 'artist'}`}>
		<h2 class="mb-3 font-heading text-xl font-bold">About {name}</h2>
		<div class="overflow-hidden rounded-2xl border bg-card/50">
			{#if photos.length}
				<div class="grid h-64 gap-1 bg-muted sm:h-80 {photos.length > 1 ? 'grid-cols-3' : ''}">
					<button
						type="button"
						class="group relative overflow-hidden text-left {photos.length > 1 ? 'col-span-2' : ''}"
						onclick={() => (lightbox = photos[0])}
						aria-label={`Open photo of ${name ?? 'artist'}`}
					>
						<img
							src={photos[0]}
							alt=""
							class="h-full w-full object-cover transition-transform duration-500 group-hover:scale-[1.02]"
							loading="lazy"
							onerror={() => photoFailed(photos[0])}
						/>
						<div class="absolute inset-0 bg-gradient-to-t from-black/45 via-transparent to-transparent"></div>
					</button>
					{#if photos.length > 1}
						<div class="grid min-w-0 grid-rows-2 gap-1">
							{#each photos.slice(1, 3) as photo (photo)}
								<button
									type="button"
									class="group min-h-0 overflow-hidden"
									onclick={() => (lightbox = photo)}
									aria-label={`Open photo of ${name ?? 'artist'}`}
								>
									<img
										src={photo}
										alt=""
										class="h-full w-full object-cover transition-transform duration-500 group-hover:scale-105"
										loading="lazy"
										onerror={() => photoFailed(photo)}
									/>
								</button>
							{/each}
						</div>
					{/if}
				</div>
			{/if}

			<div class="grid gap-7 p-6 md:grid-cols-[12rem_minmax(0,1fr)] md:p-7">
				<div class="space-y-5">
					{#if monthlyListeners}
						<div>
							<div class="font-heading text-3xl font-bold">{monthlyListeners.replace(/\s+monthly.*$/i, '')}</div>
							<div class="mt-0.5 text-sm text-muted-foreground">Monthly audience</div>
						</div>
					{/if}
					{#if subscribers}
						<div>
							<div class="font-heading text-2xl font-bold">{subscribers.replace(/\s+subscribers?$/i, '')}</div>
							<div class="mt-0.5 text-sm text-muted-foreground">YouTube subscribers</div>
						</div>
					{/if}
				</div>

				<div class="min-w-0">
					{#if description}
						<p class="whitespace-pre-line text-sm leading-6 text-foreground/85">{description}</p>
					{/if}
					{#if socialLinks.length}
						<div class="mt-6 flex flex-wrap gap-2">
							{#each socialLinks as link (link.label)}
								<button
									type="button"
									class="flex cursor-pointer items-center gap-2 rounded-full border bg-background/40 px-3.5 py-2 text-sm font-medium transition hover:border-foreground/25 hover:bg-accent/10"
									onclick={() => open(link.url)}
								>
									<HugeiconsIcon icon={link.icon} class="h-4 w-4" />
									{link.label}
									<HugeiconsIcon icon={ArrowUpRight01Icon} class="h-3.5 w-3.5 text-muted-foreground" />
								</button>
							{/each}
						</div>
					{/if}
					{#if loading}
						<p class="mt-4 text-xs text-muted-foreground/60">Finding official artist links…</p>
					{/if}
				</div>
			</div>
		</div>
	</section>
{/if}
</div>

{#if lightbox}
	<div
		class="fixed inset-0 z-[100] flex items-center justify-center bg-black/90 p-6"
		role="dialog"
		aria-modal="true"
		aria-label={`Photo of ${name ?? 'artist'}`}
	>
		<button
			type="button"
			class="absolute inset-0 cursor-default"
			onclick={() => (lightbox = null)}
			aria-label="Close photo"
		></button>
		<button
			type="button"
			class="absolute right-5 top-5 z-10 flex h-10 w-10 cursor-pointer items-center justify-center rounded-full bg-white/10 text-white transition hover:bg-white/20"
			onclick={() => (lightbox = null)}
			aria-label="Close photo"
		>
			<HugeiconsIcon icon={Cancel01Icon} class="h-5 w-5" />
		</button>
		<img
			src={lightbox}
			alt={name ?? ''}
			class="relative z-10 max-h-full max-w-full rounded-xl object-contain shadow-2xl"
			onerror={() => photoFailed(lightbox!)}
		/>
	</div>
{/if}
