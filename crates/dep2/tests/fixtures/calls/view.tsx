function clicked() {} // @fn clicked
function Button({ onClick }: { onClick: () => void }) { // @fn Button
  onClick(); // @call clicked @caller Button
  return <button />;
}
const ui = { Button };
function Page() { // @fn Page
  return <Button onClick={clicked} />; // @call Button @caller Page
}
function MemberPage() { // @fn MemberPage
  return <ui.Button onClick={clicked}></ui.Button>; // @call Button
}
