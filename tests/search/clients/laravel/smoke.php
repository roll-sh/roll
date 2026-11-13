<?php
// Laravel Scout smoke test against roll search: a stock Laravel application with Scout and
// babenkoivan/elastic-scout-driver, an Eloquent model (SQLite in memory) kept in sync by
// Scout's observers, searches through Scout's builder (query string, where / whereIn, order,
// pagination, raw results), the import / flush paths (bulk, delete by query) and the
// elastic-adapter index manager.
//
// Usage: php smoke.php   (ES_URL, default http://127.0.0.1:59200)

require __DIR__ . '/vendor/autoload.php';

use App\Models\Book;
use Elastic\Adapter\Indices\Index;
use Elastic\Adapter\Indices\IndexManager;
use Elastic\Adapter\Indices\Mapping;
use Elastic\Adapter\Indices\Settings;
use Illuminate\Database\Schema\Blueprint;
use Illuminate\Support\Facades\Artisan;
use Illuminate\Support\Facades\Schema;

$url = getenv('ES_URL') ?: 'http://127.0.0.1:59200';
putenv('SCOUT_DRIVER=elastic');
putenv("ELASTIC_HOST=$url");
putenv('DB_CONNECTION=sqlite');
putenv('DB_DATABASE=:memory:');

$app = require __DIR__ . '/bootstrap/app.php';
$app->make(Illuminate\Contracts\Console\Kernel::class)->bootstrap();
config([
    'scout.driver' => 'elastic',
    'scout.queue' => false,
    'elastic.client.connections.default.hosts' => [$url],
    'elastic.scout_driver.refresh_documents' => true,
    'database.default' => 'sqlite',
    'database.connections.sqlite.database' => ':memory:',
]);

const IDX = 'clients-laravel-books';
$failures = [];

function check(string $name, callable $fn): void
{
    global $failures;
    try {
        $fn();
        echo "ok   $name\n";
    } catch (Throwable $e) {
        $failures[] = $name;
        echo "FAIL $name: " . get_class($e) . ': ' . $e->getMessage() . "\n";
    }
}

function eq($got, $want, string $what): void
{
    if ($got !== $want) {
        throw new RuntimeException("$what: expected " . json_encode($want) . ', got ' . json_encode($got));
    }
}

Schema::create('books', function (Blueprint $t) {
    $t->id();
    $t->string('title');
    $t->string('author');
    $t->integer('year');
    $t->string('genre');
});

$indices = app(IndexManager::class);

check('client info and product check', function () {
    // The client elastic-client builds from config/elastic.client.php (the driver's).
    $info = app(Elastic\Client\ClientBuilderInterface::class)->default()->info()->asArray();
    eq($info['version']['number'], '9.5.5', 'version');
});

check('create index through the index manager', function () use ($indices) {
    if ($indices->exists(IDX)) {
        $indices->drop(IDX);
    }
    $mapping = (new Mapping())
        ->text('title', ['fields' => ['raw' => ['type' => 'keyword']]])
        ->text('author')
        ->integer('year')
        ->keyword('genre');
    $settings = (new Settings())->index(['number_of_shards' => 2, 'number_of_replicas' => 0]);
    $indices->create(new Index(IDX, $mapping, $settings));
    eq($indices->exists(IDX), true, 'exists');
});

$books = [
    ['Dune', 'Frank Herbert', 1965, 'scifi'],
    ['Foundation', 'Isaac Asimov', 1951, 'scifi'],
    ['The Hobbit', 'J. R. R. Tolkien', 1937, 'fantasy'],
    ['Neuromancer', 'William Gibson', 1984, 'scifi'],
    ['A Wizard of Earthsea', 'Ursula K. Le Guin', 1968, 'fantasy'],
    ['The Left Hand of Darkness', 'Ursula K. Le Guin', 1969, 'scifi'],
    ['Children of Dune', 'Frank Herbert', 1976, 'scifi'],
];

check('models sync through the observer', function () use ($books) {
    foreach ($books as [$title, $author, $year, $genre]) {
        Book::create(compact('title', 'author', 'year', 'genre'));
    }
    eq(Book::search()->get()->count(), 7, 'match_all');
});

check('query string search', function () {
    eq(Book::search('dune')->get()->pluck('title')->sort()->values()->all(), ['Children of Dune', 'Dune'], 'dune');
    eq(Book::search('title:(Foundation OR Neuromancer)')->get()->pluck('title')->sort()->values()->all(), ['Foundation', 'Neuromancer'], 'mini-language');
    eq(Book::search('le guin')->get()->count(), 2, 'author');
});

check('where, whereIn, whereNotIn and orderBy', function () {
    eq(Book::search()->where('genre', 'fantasy')->orderBy('year')->get()->pluck('title')->all(), ['The Hobbit', 'A Wizard of Earthsea'], 'where');
    eq(Book::search()->whereIn('year', [1965, 1984])->orderBy('year', 'desc')->get()->pluck('title')->all(), ['Neuromancer', 'Dune'], 'whereIn');
    eq(Book::search()->where('genre', 'scifi')->whereNotIn('year', [1965, 1976])->get()->count(), 3, 'whereNotIn');
    eq(Book::search('herbert')->where('genre', 'scifi')->orderBy('year')->keys()->count(), 2, 'keys');
});

check('pagination and raw results', function () {
    $p = Book::search()->orderBy('year')->paginate(3, 'page', 2);
    eq($p->total(), 7, 'total');
    eq($p->lastPage(), 3, 'last page');
    eq(collect($p->items())->pluck('title')->all(), ['A Wizard of Earthsea', 'The Left Hand of Darkness', 'Children of Dune'], 'page 2');
    $raw = Book::search('dune')->raw();
    eq($raw->total(), 2, 'raw total');
    eq(count($raw->hits()), 2, 'raw hits');
});

check('update and delete keep the index in sync', function () {
    $b = Book::where('title', 'Foundation')->first();
    $b->update(['title' => 'Foundation and Empire', 'year' => 1952]);
    eq(Book::search('empire')->get()->pluck('year')->all(), [1952], 'updated');
    $b->delete();
    eq(Book::search('foundation')->get()->count(), 0, 'deleted');
    eq(Book::search()->get()->count(), 6, 'count after delete');
});

check('unsearchable, flush (delete by query) and import (bulk)', function () {
    Book::where('genre', 'fantasy')->get()->unsearchable();
    eq(Book::search()->get()->count(), 4, 'after unsearchable');
    Book::removeAllFromSearch();
    eq(Book::search()->get()->count(), 0, 'after flush');
    Book::makeAllSearchable();
    eq(Book::search()->get()->count(), 6, 'after makeAllSearchable');
    Artisan::call('scout:flush', ['model' => Book::class]);
    eq(Book::search()->get()->count(), 0, 'after scout:flush');
    Artisan::call('scout:import', ['model' => Book::class]);
    eq(Book::search()->get()->count(), 6, 'after scout:import');
});

check('drop index', function () use ($indices) {
    $indices->drop(IDX);
    eq($indices->exists(IDX), false, 'dropped');
});

if ($failures) {
    echo count($failures) . " check(s) failed\n";
    exit(1);
}
echo "laravel-scout: all checks passed\n";
