<?php

namespace App\Models;

use Illuminate\Database\Eloquent\Model;
use Laravel\Scout\Searchable;

class Book extends Model
{
    use Searchable;

    protected $fillable = ['title', 'author', 'year', 'genre'];

    public $timestamps = false;

    public function searchableAs(): string
    {
        return 'clients-laravel-books';
    }

    public function toSearchableArray(): array
    {
        return [
            'title' => $this->title,
            'author' => $this->author,
            'year' => $this->year,
            'genre' => $this->genre,
        ];
    }
}
