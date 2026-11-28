import { createFileRoute, notFound } from "@tanstack/react-router";
import { MarketingFooter, MarketingHeader } from "@/components/marketing/chrome";
import { ProductConnect, ProductDashboard, ProductDemo, ProductFacts, ProductFeatures, ProductHero, ProductRelated, ProductStart } from "@/components/marketing/product-page";
import { pageHead } from "@/lib/head";
import { productBySlug } from "@/lib/marketing";

export const Route = createFileRoute("/products/$product")({
  loader: ({ params }) => {
    const product = productBySlug(params.product);
    if (!product) throw notFound();
    return { slug: product.slug };
  },
  head: ({ loaderData }) => {
    const p = loaderData && productBySlug(loaderData.slug);
    return p ? pageHead({ title: p.name, description: `${p.tagline} ${p.blurb}` }) : {};
  },
  component: ProductPage,
});

function ProductPage() {
  const { slug } = Route.useLoaderData();
  const p = productBySlug(slug)!;
  return (
    <>
      <MarketingHeader />
      <main className="flex-1">
        <ProductHero p={p} />
        <ProductFacts p={p} />
        <ProductConnect p={p} />
        <ProductDashboard p={p} />
        <ProductDemo p={p} />
        <ProductFeatures p={p} />
        <ProductStart p={p} />
        <ProductRelated p={p} />
      </main>
      <MarketingFooter />
    </>
  );
}
