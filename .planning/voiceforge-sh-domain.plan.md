---
roadmap_item: 5.1.1 voiceforge.sh on Cloudflare Pages
status: ready when you are — needs Cloudflare account + domain
---

# Attaching voiceforge.sh to GitHub Pages

This is the post-launch upgrade that turns
`https://humancto.github.io/voice-forge/` into `https://voiceforge.sh/`.
Phase 1 (the GH Pages workflow) already shipped in PR #38; this plan
just describes the domain-attach step.

## You already have

- `.github/workflows/pages.yml` building `_site/` and deploying to
  GitHub Pages on push-to-main.
- A working site at `https://humancto.github.io/voice-forge/` once
  you flip Settings → Pages → Source: "GitHub Actions".

## What you need to do (one-time, ~5 min)

### Option A: keep on GitHub Pages, just attach the custom domain

1. **Buy / point** `voiceforge.sh` (Cloudflare Registrar, Namecheap,
   wherever). The TLD `.sh` registrar is the Saint Helena gov; ~$60/yr
   typical pricing.

2. **DNS records** at your registrar (or Cloudflare DNS):

   ```
   voiceforge.sh        A     185.199.108.153
   voiceforge.sh        A     185.199.109.153
   voiceforge.sh        A     185.199.110.153
   voiceforge.sh        A     185.199.111.153
   www.voiceforge.sh    CNAME humancto.github.io
   ```

   (GitHub Pages IPs; current as of this writing — verify at
   https://docs.github.com/en/pages/configuring-a-custom-domain-for-your-github-pages-site)

3. **GitHub repo Settings → Pages → Custom domain**: enter
   `voiceforge.sh`, save. GitHub provisions a Let's Encrypt cert
   (~30s). Tick "Enforce HTTPS" once the cert is live.

4. **Add a `CNAME` file** in `docs/` so future deploys preserve the
   custom-domain config:

   ```bash
   echo voiceforge.sh > docs/CNAME
   git add docs/CNAME && git commit -m "chore(pages): add CNAME for voiceforge.sh" && git push
   ```

5. **Update README + docs** to reference `voiceforge.sh` as primary;
   the `humancto.github.io` URL keeps working as a 301 redirect.

Done. Total cost: domain reg + 5 min of clicking. Zero infrastructure
to babysit.

### Option B: migrate to Cloudflare Pages (more control, more setup)

Only worth it if you want CF Workers in front, custom 404s, or
preview deploys per PR. Otherwise Option A is strictly simpler.

Quick steps:

1. Cloudflare Dashboard → Pages → Connect to Git → select
   `humancto/voice-forge`
2. Build settings:
   - Framework preset: **None**
   - Build command: `mkdir -p _site && cp -R docs/. _site/ && cp install.sh _site/install.sh`
   - Build output directory: `_site`
3. Custom domain: add `voiceforge.sh`, follow CF DNS guidance
4. Delete `.github/workflows/pages.yml` from this repo (CF Pages
   handles deploy)

Trade-off: Cloudflare gets the deploy pipeline; you lose the GH
Actions workflow ergonomics; you gain CF analytics + Workers if you
ever want them.

## Recommendation

**Option A.** GH Pages + custom domain is the cheapest, most boring,
zero-maintenance path. The GH Pages workflow we just shipped already
serves install.sh under the same domain — nothing to migrate.
