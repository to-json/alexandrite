# Every product a*b, 100 <= a <= b <= 999, checked; no pruning (same work as pe004.alx).
best = 0
a = 100
while a <= 999
  b = a
  while b <= 999
    p = a * b
    # numeric reversal instead of to_s.reverse: no allocation
    r = 0
    m = p
    while m > 0
      r = r * 10 + m % 10
      m /= 10
    end
    best = p if r == p && p > best
    b += 1
  end
  a += 1
end
puts best
